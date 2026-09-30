//! Windows process-tree ownership. Agent children stay suspended until their
//! kill-on-close job is assigned; PTY children retain their existing attach path.
use super::*;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

pub(crate) struct KillOnCloseJob(OwnedHandle);

impl KillOnCloseJob {
    fn new() -> std::io::Result<Self> {
        // SAFETY: null attributes/name create a private, non-inheritable job.
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a successful CreateJobObjectW returns an owned handle.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(raw) });
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: the pointer and size describe the initialized limits structure.
        if unsafe {
            SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, process: HANDLE) -> std::io::Result<()> {
        // SAFETY: callers keep a valid process handle alive throughout this call.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn attach(pid: u32) -> Option<Self> {
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };
        let job = Self::new().ok()?;
        // SAFETY: OpenProcess validates the pid; no handle inheritance is requested.
        let raw = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if raw.is_null() {
            return None;
        }
        // SAFETY: OpenProcess returned a new owned handle.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        job.assign(process.as_raw_handle()).ok()?;
        Some(job)
    }

    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<(std::process::Child, Self)> {
        let job = Self::new()?;
        let child = job.spawn_child(command)?;
        Ok((child, job))
    }

    fn spawn_child(&self, command: &mut Command) -> std::io::Result<std::process::Child> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        command.creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        // The child cannot execute user code or spawn descendants before it is
        // assigned. Every failure kills and reaps the still-owned child.
        if let Err(error) = self
            .assign(child.as_raw_handle())
            .and_then(|()| resume_suspended_child(&child))
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok(child)
    }

    pub(crate) fn terminate(&self) {
        // SAFETY: the job handle is valid for this call. Terminate the tree before
        // taking the reader's child mutex, which may be held across child.wait().
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }
}

/// Stable Rust does not expose Child's primary thread handle. A newly created
/// suspended process has one thread; find that thread without resuming any
/// unrelated process. The Child handle keeps its process identity alive.
fn resume_suspended_child(child: &std::process::Child) -> std::io::Result<()> {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessIdOfThread, OpenThread, ResumeThread, THREAD_QUERY_LIMITED_INFORMATION,
        THREAD_SUSPEND_RESUME,
    };
    // SAFETY: a thread snapshot has no input buffers and returns an owned handle.
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful snapshot handle is owned and closed on every exit.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of_val(&entry) as u32;
    // SAFETY: entry is initialized with the required structure size.
    let mut available = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while available != 0 {
        if entry.th32OwnerProcessID == child.id() {
            // SAFETY: OpenThread validates the id; the returned handle is owned.
            let raw = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
                    0,
                    entry.th32ThreadID,
                )
            };
            if raw.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
            // SAFETY: the handle has query and resume rights. Verify ownership
            // again in case the snapshot became stale before OpenThread.
            if unsafe { GetProcessIdOfThread(thread.as_raw_handle()) } != child.id() {
                return Err(std::io::Error::other("agent thread identity changed"));
            }
            let count = unsafe { ResumeThread(thread.as_raw_handle()) };
            if count == u32::MAX {
                return Err(std::io::Error::last_os_error());
            }
            return if count == 1 {
                Ok(())
            } else {
                Err(std::io::Error::other(
                    "unexpected agent thread suspend count",
                ))
            };
        }
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        // SAFETY: snapshot and output structure remain valid.
        available = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    Err(std::io::Error::other("suspended agent thread not found"))
}

#[cfg(test)]
mod windows_job_tests {
    use super::*;
    use windows_sys::Win32::Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows_sys::Win32::System::JobObjects::IsProcessInJob;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    };

    const FIXTURE: &str = "windows_job::windows_job_tests::windows_job_fixture";

    #[test]
    fn windows_job_fixture() {
        let Ok(phase) = std::env::var("SGIAN_JOB_FIXTURE_PHASE") else {
            return;
        };
        let dir = PathBuf::from(std::env::var_os("SGIAN_JOB_FIXTURE_DIR").expect("fixture dir"));
        if phase == "parent" {
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", FIXTURE, "--nocapture"])
                .env("SGIAN_JOB_FIXTURE_PHASE", "child")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("immediate grandchild");
            fs::write(dir.join("grandchild.pid"), child.id().to_string()).expect("pid marker");
            // Bounded fallback if the parent test aborts before killing the job.
            let _ = child.wait();
        } else {
            fs::write(dir.join("started"), b"running").expect("started marker");
            thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn windows_agent_job_owns_immediate_descendants_before_session_commit() {
        let dir = tempfile::tempdir().expect("fixture dir");
        let exe = std::env::current_exe().expect("test executable");
        // Exercise the same cmd.exe shim path as npm-installed agent CLIs.
        let shim = dir.path().join("fixture.cmd");
        fs::write(
            &shim,
            format!(
                "@echo off\r\n\"{}\" --exact {FIXTURE} --nocapture\r\n",
                exe.display()
            ),
        )
        .expect("shim");
        let prepared = execute_agent_spawn(&AgentSpawnPlan {
            backend: AgentBackendKind::Claude,
            bin: AgentBinPlan::ViaCmd(shim),
            args: Vec::new(),
            env: HashMap::from([
                ("SGIAN_JOB_FIXTURE_PHASE".into(), "parent".into()),
                (
                    "SGIAN_JOB_FIXTURE_DIR".into(),
                    dir.path().display().to_string(),
                ),
            ]),
            scrub_env: Vec::new(),
            cwd: dir.path().to_path_buf(),
            command_str: "job test".into(),
            cwd_str: dir.path().display().to_string(),
            initial_input: None,
        })
        .expect("agent spawn");
        let deadline = Instant::now() + Duration::from_secs(15);
        let pid: u32 = loop {
            let pid = fs::read_to_string(dir.path().join("grandchild.pid"))
                .ok()
                .and_then(|value| value.parse().ok());
            if dir.path().join("started").exists() {
                if let Some(pid) = pid {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "grandchild never started");
            thread::sleep(Duration::from_millis(10));
        };
        // SAFETY: the fixture owns this live process; the handle only queries/waits.
        let raw = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        assert!(!raw.is_null(), "grandchild process handle");
        let grandchild = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut in_job = 0;
        // Check THIS job, not an enclosing job installed by the CI runner.
        assert_ne!(
            unsafe {
                IsProcessInJob(
                    grandchild.as_raw_handle(),
                    prepared.job.0.as_raw_handle(),
                    &mut in_job,
                )
            },
            0
        );
        assert_ne!(in_job, 0, "grandchild escaped the agent job");
        assert_eq!(
            unsafe { WaitForSingleObject(grandchild.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        let child = prepared.child;
        let killer = AgentChildKiller {
            child: Arc::new(Mutex::new(child)),
            job: prepared.job,
        };
        killer.kill();
        assert_eq!(
            unsafe { WaitForSingleObject(grandchild.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
        assert!(killer.child.lock().expect("child").wait().is_ok());
    }

    #[test]
    fn windows_agent_job_assignment_failure_never_runs_child_code() {
        use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        let job = KillOnCloseJob::new().expect("job");
        let dir = tempfile::tempdir().expect("fixture dir");
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        limits.BasicLimitInformation.ActiveProcessLimit = 0;
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    job.0.as_raw_handle(),
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            },
            0
        );
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--exact", FIXTURE, "--nocapture"])
            .env("SGIAN_JOB_FIXTURE_PHASE", "child")
            .env("SGIAN_JOB_FIXTURE_DIR", dir.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        assert!(
            job.spawn_child(&mut command).is_err(),
            "job must refuse the child"
        );
        assert!(
            !dir.path().join("started").exists(),
            "child ran before assignment"
        );
    }

    #[test]
    fn windows_agent_job_drop_kills_a_spawn_that_was_never_committed() {
        let mut command = Command::new("cmd.exe");
        command.args(["/d", "/c", "ping -n 30 127.0.0.1 >nul"]);
        let (mut child, job) = KillOnCloseJob::spawn(&mut command).expect("spawn");
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 0) },
            WAIT_TIMEOUT
        );
        drop(job);
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle(), 5000) },
            WAIT_OBJECT_0
        );
        child.wait().expect("reap child");
    }
}
