use super::*;

/// Every descendant of `root` in one `ps` snapshot (root excluded).
#[cfg(unix)]
pub(crate) fn process_descendants(root: u32, table: &ProcessTable) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, ppid) in &table.parent {
        children.entry(*ppid).or_default().push(*pid);
    }
    let mut found = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(root);
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        if let Some(kids) = children.get(&pid) {
            for kid in kids {
                if seen.insert(*kid) {
                    found.push(*kid);
                    queue.push(*kid);
                }
            }
        }
    }
    found
}

/// Read a per-process start-time token from the kernel so delayed `SIGKILL`
/// can verify the PID was not recycled during the grace window.
#[cfg(target_os = "macos")]
fn process_start_token(pid: u32) -> Option<u64> {
    unsafe {
        let mut info: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(
            pid as libc::pid_t,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        );
        if got == size {
            Some(
                info.pbi_start_tvsec
                    .wrapping_mul(1_000_000)
                    .wrapping_add(info.pbi_start_tvusec),
            )
        } else {
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn process_start_token(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(19)?.parse::<u64>().ok()
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn process_start_token(_pid: u32) -> Option<u64> {
    None
}

/// Terminate everything under a pane's child, not only the shell: an
/// interactive shell puts each job in its own process group, so killing the
/// shell alone orphans an agent started from it. Descendants get SIGTERM now
/// and SIGKILL after a grace period if still alive.
/// Child → parent for every process, read from the kernel without forking:
/// libproc on macOS, /proc on Linux. Forking here would be wrong twice over:
/// a pane close would pay a `ps` per session, and a forked child briefly
/// holds duplicates of every fd, which keeps an advisory `flock` alive past
/// its owner's drop (the daemon lock probe races that window).
#[cfg(target_os = "macos")]
pub(crate) fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    // SAFETY: proc_listallpids sizes its answer to the buffer we pass, and
    // proc_pidinfo writes at most `size_of::<proc_bsdinfo>()` bytes into a
    // zeroed struct we own; every pointer is valid for the call's duration.
    unsafe {
        let needed = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if needed <= 0 {
            return None;
        }
        let mut pids = vec![0 as libc::pid_t; needed as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        let count = libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes);
        if count <= 0 {
            return None;
        }
        pids.truncate(count as usize);
        let mut parents = HashMap::with_capacity(pids.len());
        for pid in pids {
            if pid <= 0 {
                continue;
            }
            let mut info: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            let got = libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            );
            if got == size {
                parents.insert(pid as u32, info.pbi_ppid);
            }
        }
        Some(parents)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    let mut parents = HashMap::new();
    for entry in fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid …` — comm may contain spaces or parens, so
        // split after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        fields.next(); // state
        if let Some(ppid) = fields.next().and_then(|field| field.parse().ok()) {
            parents.insert(pid, ppid);
        }
    }
    Some(parents)
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(crate) fn process_parent_snapshot() -> Option<HashMap<u32, u32>> {
    None
}

/// The process tree for hook placement: the fork-free kernel snapshot on
/// macOS and Linux; nothing elsewhere (a hook there reports `mapped: false`).
#[cfg(unix)]
pub(crate) fn process_parent_snapshot_for_hooks() -> Option<HashMap<u32, u32>> {
    process_parent_snapshot()
}

#[cfg(not(unix))]
pub(crate) fn process_parent_snapshot_for_hooks() -> Option<HashMap<u32, u32>> {
    None
}

#[cfg(unix)]
pub(crate) fn terminate_process_tree(root: u32) {
    let parent = match process_parent_snapshot() {
        Some(parent) => parent,
        None => {
            // Last resort on other Unixes: a `ps` snapshot (forks once).
            let Ok(output) = Command::new("ps")
                .args(["-axo", "pid=,ppid="])
                .stdin(Stdio::null())
                .output()
            else {
                return;
            };
            parse_process_table(&String::from_utf8_lossy(&output.stdout)).parent
        }
    };
    let table = ProcessTable {
        parent,
        args: HashMap::new(),
    };
    let targets = process_descendants(root, &table);
    if targets.is_empty() {
        return;
    }
    let targets_with_start: Vec<(u32, Option<u64>)> = targets
        .into_iter()
        .map(|pid| (pid, process_start_token(pid)))
        .collect();
    for &(pid, _) in &targets_with_start {
        // SAFETY: kill(2) with a pid we just read from the process table; a
        // stale pid is an ESRCH we ignore.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(1500));
        for (pid, start_token) in targets_with_start {
            if let Some(expected) = start_token {
                if process_start_token(pid) != Some(expected) {
                    continue;
                }
            }
            // SAFETY: as above; signal 0 only probes existence.
            unsafe {
                if libc::kill(pid as libc::pid_t, 0) == 0 {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    });
}
