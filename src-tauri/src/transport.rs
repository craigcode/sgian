use super::*;

// ---------------------------------------------------------------------------
// Transport abstraction: Unix domain sockets (cfg(unix)) vs Windows named
// pipes (cfg(windows)).
//
// The proven Unix implementation is kept intact behind cfg(unix). A
// cfg(windows) named-pipe implementation using windows-sys
// (CreateNamedPipeW / ConnectNamedPipe / CreateFileW) is provided so the
// crate cross-compiles for x86_64-pc-windows-gnu. The transport trait
// abstracts connect / listen / accept (including non-blocking WouldBlock
// idle semantics), addressing, permissioning, and stale-handle cleanup.
//
// Windows RUNTIME behavior is unvalidated by agreement — the bound is
// `cargo check --target x86_64-pc-windows-gnu` compiles with 0 errors.
// ---------------------------------------------------------------------------

/// The connected-stream type used by the IPC transport layer.
///
/// On Unix this is `std::os::unix::net::UnixStream`; on Windows it is a
/// named-pipe handle wrapper (`WindowsNamedPipeStream`). Both implement
/// `Read` + `Write` and provide `try_clone`, `shutdown`, `set_write_timeout`,
/// `set_read_timeout`, and `set_nonblocking` as inherent methods so the
/// calling code is identical across platforms via this alias.
#[cfg(unix)]
pub(crate) type TransportStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
pub(crate) type TransportStream = WindowsNamedPipeStream;

/// The listener type used by the daemon's accept loop.
///
/// On Unix this is `std::os::unix::net::UnixListener`; on Windows it is a
/// named-pipe server (`WindowsNamedPipeListener`). Both provide `accept()`
/// (returning `(TransportStream, ())`) and `set_nonblocking(bool)`.
#[cfg(unix)]
pub(crate) type TransportListener = std::os::unix::net::UnixListener;
#[cfg(windows)]
pub(crate) type TransportListener = WindowsNamedPipeListener;

/// Connect to the transport endpoint at `path`.
///
/// On Unix, `path` is a filesystem socket path (`UnixStream::connect`).
/// On Windows, a named-pipe name is derived from `path` and opened via
/// `CreateFileW`.
pub(crate) fn transport_connect(path: &Path) -> std::io::Result<TransportStream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path)
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeStream::connect(path)
    }
}

/// Return the concrete endpoint a native client should connect to. Unix
/// clients receive the domain-socket path; Windows clients receive the
/// owner-scoped named-pipe path derived by the same code the daemon uses.
pub(crate) fn transport_endpoint(path: &Path) -> std::io::Result<String> {
    #[cfg(unix)]
    {
        Ok(path.display().to_string())
    }
    #[cfg(windows)]
    {
        windows_transport::pipe_name_from_path(path)
    }
}

/// Bind a transport listener at `path`.
///
/// On Unix, `path` is a filesystem socket path (`UnixListener::bind`).
/// On Windows, a named-pipe server is created via `CreateNamedPipeW`.
pub(crate) fn transport_bind(path: &Path) -> std::io::Result<TransportListener> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixListener::bind(path)
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeListener::bind(path)
    }
}

/// Compose a Windows named-pipe name scoped to BOTH the workspace socket path
/// (hashed) AND a per-user component (the caller's user-SID string on Windows).
/// Two different users therefore derive distinct pipe names for the same
/// workspace, so they cannot collide on (or hijack) each other's pipe. Factored
/// out and platform-independent so the SID-scoping contract is unit-testable on
/// any host; the live SID lookup that feeds `sid_component` is Windows-only.
#[cfg(any(windows, test))]
pub(crate) fn pipe_name_with_sid(sid_component: &str, path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    // Keep only pipe-name-safe characters; SID strings ("S-1-5-21-...") already
    // satisfy this, but guard against anything unexpected from the OS.
    let safe_sid: String = sid_component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(
        "\\\\.\\pipe\\{WINDOWS_IPC_NAMESPACE}-{}-{:016x}",
        safe_sid,
        hasher.finish()
    )
}

/// Compose the per-user pipe name from a resolved SID, failing CLOSED when the
/// per-user SID could not be resolved. A Windows pipe name MUST always embed a
/// real per-user SID — there is NO placeholder/"nosid" fallback, because a fixed
/// component would let two users collide on (or hijack) the same workspace pipe.
/// Platform-independent so the fail-closed contract is unit-testable on any host;
/// the live SID lookup that feeds it is Windows-only.
#[cfg(any(windows, test))]
pub(crate) fn pipe_name_from_sid(sid: Option<String>, path: &Path) -> std::io::Result<String> {
    match sid {
        Some(sid) => Ok(pipe_name_with_sid(&sid, path)),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to build a Windows pipe name without a per-user SID",
        )),
    }
}

/// Fail-closed guard for the owner-restricted pipe security descriptor. The pipe
/// MUST NOT be created with default/inherited ACLs, so a NULL descriptor (the SID
/// lookup or the SDDL build failed) is refused — on BOTH the initial bind and each
/// post-accept recreate — rather than silently downgraded. Platform-independent so
/// the fail-closed contract is unit-testable on any host.
#[cfg(any(windows, test))]
pub(crate) fn require_owner_descriptor(descriptor_is_null: bool) -> std::io::Result<()> {
    if descriptor_is_null {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refusing to create a named pipe without an owner-restricted security descriptor",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Windows named-pipe transport implementation (cfg(windows) only).
//
// Uses windows-sys FFI: CreateNamedPipeW (server), CreateFileW (client),
// ConnectNamedPipe (accept), ReadFile/WriteFile (I/O), DuplicateHandle
// (try_clone). Non-blocking accept uses overlapped I/O so WouldBlock is
// returned when no client is pending. Stream I/O is overlapped too (the pipe
// handles are opened with FILE_FLAG_OVERLAPPED), with per-direction
// OVERLAPPED/event state shared across clones and deadline-bounded waits so
// set_read_timeout/set_write_timeout are honored (H5).
// ---------------------------------------------------------------------------
