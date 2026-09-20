//! The unit and in-process integration suite, split by area; `mod.rs` holds
//! the transport helpers every file shares. Each file opens with
//! `use super::*` and is re-imported here so helpers stay visible across files.

use super::*;

#[cfg(unix)]
use std::os::unix::net::UnixStream;

/// Create a connected in-process transport pair on every supported host.
/// Unix has a native socketpair; Windows connects to a uniquely named pipe
/// instance and accepts it before dropping the temporary listener.
pub(crate) fn test_transport_pair() -> std::io::Result<(TransportStream, TransportStream)> {
    #[cfg(unix)]
    {
        return UnixStream::pair();
    }
    #[cfg(windows)]
    {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("transport-pair.sock");
        let listener = transport_bind(&path)?;
        let client = transport_connect(&path)?;
        let (server, _) = listener.accept()?;
        return Ok((client, server));
    }
    #[allow(unreachable_code)]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "no test transport for this platform",
    ))
}

#[test]
fn control_invocation_accepts_current_and_legacy_binary_names() {
    assert!(is_control_invocation(&[
        "sgian".to_string(),
        "ctl".to_string()
    ]));
    assert!(is_control_invocation(&["/tmp/sgianctl".to_string()]));
    assert!(is_control_invocation(&["/tmp/sgian2ctl".to_string()]));
    assert!(!is_control_invocation(&["/tmp/sgian".to_string()]));
}

#[test]
fn new_registry_starts_with_one_terminal_pane() {
    let registry = PaneRegistry::new("/tmp/sgian".to_string());
    let snapshot = registry.snapshot();

    assert_eq!(snapshot.panes.len(), 1);
    assert_eq!(snapshot.active_pane_id, Some("pane-1".to_string()));
    assert_eq!(snapshot.panes[0].title, "term-1");
}

#[test]
fn create_pane_assigns_distinct_ids_and_focuses_latest() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let pane = registry.create_pane(Some("editor".to_string()));
    let snapshot = registry.snapshot();

    assert_eq!(pane.id, "pane-2");
    assert_eq!(pane.title, "editor");
    assert_eq!(snapshot.active_pane_id, Some("pane-2".to_string()));
    assert_eq!(snapshot.panes.len(), 2);
}

#[test]
fn close_pane_keeps_at_least_one_pane() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let err = registry
        .close_pane("pane-1")
        .expect_err("last pane should stay open");

    assert_eq!(err, "at least one pane must remain open");
    assert_eq!(registry.snapshot().panes.len(), 1);
}

#[test]
fn rename_pane_trims_title() {
    let mut registry = PaneRegistry::new("/tmp/sgian".to_string());
    let pane = registry
        .rename_pane("pane-1", "  build  ".to_string())
        .expect("pane should rename");

    assert_eq!(pane.title, "build");
}

#[test]
fn terminal_store_remembers_size_before_session_exists() {
    let mut store = TerminalStore::new_for_tests(PathBuf::from("/tmp/sgian"));

    store
        .resize_pane("pane-1", 101, 31)
        .expect("resize should be remembered");

    let size = store
        .sizes
        .get("pane-1")
        .expect("size should be available for later spawn");
    assert_eq!(size.cols, 101);
    assert_eq!(size.rows, 31);
}

#[test]
fn pty_size_clamps_dimensions_from_both_sides() {
    let size = pty_size(0, 0);
    assert_eq!((size.cols, size.rows), (2, 1));

    // A hostile 65535x65535 resize must not reach the vt100 model (H4).
    let size = pty_size(u16::MAX, u16::MAX);
    assert_eq!((size.cols, size.rows), (MAX_PTY_COLS, MAX_PTY_ROWS));

    let size = pty_size(120, 40);
    assert_eq!((size.cols, size.rows), (120, 40));
}

#[test]
fn resize_pane_stores_clamped_dimensions() {
    let mut store = TerminalStore::new_for_tests(PathBuf::from("/tmp/sgian"));

    store
        .resize_pane("pane-1", u16::MAX, u16::MAX)
        .expect("oversized resize should clamp, not fail");

    let size = store.sizes.get("pane-1").expect("size should be stored");
    assert_eq!(size.cols, MAX_PTY_COLS);
    assert_eq!(size.rows, MAX_PTY_ROWS);
}

#[test]
fn queue_pane_input_fails_fast_when_pane_stops_draining() {
    // A writer that blocks forever, like a PTY whose foreground process
    // stopped reading stdin (Ctrl-S / stopped job) with a full kernel buffer.
    // Requests must fail fast with a backlog error, never block (H2).
    struct BlockedWriter {
        release: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }
    impl Write for BlockedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let (lock, cvar) = &*self.release;
            let mut released = lock.lock().unwrap();
            while !*released {
                released = cvar.wait(released).unwrap();
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let release = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let sender = spawn_input_writer(Box::new(BlockedWriter {
        release: Arc::clone(&release),
    }));

    // The writer thread can hold at most one in-flight chunk; the queue holds
    // PANE_INPUT_QUEUE_LIMIT more. One extra send must fail fast.
    let mut backlogged = None;
    for _ in 0..=(PANE_INPUT_QUEUE_LIMIT + 1) {
        if let Err(error) = queue_pane_input(&sender, "pane-1", "x") {
            backlogged = Some(error);
            break;
        }
    }
    let error = backlogged.expect("a full queue should fail fast, not block");
    assert!(error.contains("backlogged"), "unexpected error: {error}");

    // Unblock the writer so the thread drains and exits at teardown.
    let (lock, cvar) = &*release;
    *lock.lock().unwrap() = true;
    cvar.notify_all();
}

#[test]
fn queue_pane_input_reports_ended_after_writer_exit() {
    struct FailingWriter;
    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty gone",
            ))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let sender = spawn_input_writer(Box::new(FailingWriter));
    // The writer thread errors on its first write and exits; once the
    // receiver is dropped, sends observe Disconnected → "session ended".
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match queue_pane_input(&sender, "pane-1", "x") {
            Err(error) => {
                assert!(error.contains("session ended"), "unexpected error: {error}");
                break;
            }
            Ok(()) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(()) => panic!("writer exit should surface as a send error"),
        }
    }
}

// Every file's helpers are visible to every other file; a re-export that no
// other file uses yet is still the contract.
mod terminal;
#[allow(unused_imports)]
pub(crate) use terminal::*;
mod env_scrub;
#[allow(unused_imports)]
pub(crate) use env_scrub::*;
mod ctl_parsers;
#[allow(unused_imports)]
pub(crate) use ctl_parsers::*;
mod harness;
#[allow(unused_imports)]
pub(crate) use harness::*;
mod ctl_run;
#[allow(unused_imports)]
pub(crate) use ctl_run::*;
mod observability;
#[allow(unused_imports)]
pub(crate) use observability::*;
mod lifecycle;
#[allow(unused_imports)]
pub(crate) use lifecycle::*;
mod config;
#[allow(unused_imports)]
pub(crate) use config::*;
mod agents;
#[allow(unused_imports)]
pub(crate) use agents::*;
mod lease_identity;
#[allow(unused_imports)]
pub(crate) use lease_identity::*;
