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
/// `Local` wraps the primary owner-restricted OS IPC stream (`UnixStream` on
/// Unix, `WindowsNamedPipeStream` on Windows). `LoopbackTcp` wraps an opt-in
/// `127.0.0.1` stream used when a remote host forwards SSH tunnels to a
/// loopback TCP listener (#116) or when a Windows client connects to a local
/// SSH tunnel port (#114). Loopback TCP connections carry no kernel peer-UID or
/// owner-SID guarantee and therefore MUST present a valid per-client
/// `client_token` (`ClientIdentity`) during the `IpcHello` handshake.
#[derive(Debug)]
pub(crate) enum TransportStream {
    #[cfg(unix)]
    Local(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Local(WindowsNamedPipeStream),
    LoopbackTcp(std::net::TcpStream),
}

impl TransportStream {
    pub(crate) fn is_loopback_tcp(&self) -> bool {
        matches!(self, Self::LoopbackTcp(_))
    }

    #[cfg(unix)]
    pub(crate) fn local_unix_stream(&self) -> Option<&std::os::unix::net::UnixStream> {
        match self {
            Self::Local(stream) => Some(stream),
            Self::LoopbackTcp(_) => None,
        }
    }

    pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
        match self {
            Self::Local(stream) => stream.try_clone().map(Self::Local),
            Self::LoopbackTcp(stream) => stream.try_clone().map(Self::LoopbackTcp),
        }
    }

    pub(crate) fn shutdown(&self, how: std::net::Shutdown) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.shutdown(how),
            Self::LoopbackTcp(stream) => stream.shutdown(how),
        }
    }

    pub(crate) fn set_write_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_write_timeout(dur),
            Self::LoopbackTcp(stream) => stream.set_write_timeout(dur),
        }
    }

    pub(crate) fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_read_timeout(dur),
            Self::LoopbackTcp(stream) => stream.set_read_timeout(dur),
        }
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.set_nonblocking(nonblocking),
            Self::LoopbackTcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }

    #[cfg(windows)]
    pub(crate) fn is_peer_disconnected(&self) -> bool {
        match self {
            Self::Local(pipe) => pipe.is_peer_disconnected(),
            Self::LoopbackTcp(tcp) => {
                let _ = tcp.set_nonblocking(true);
                let mut byte = [0u8; 1];
                let disconnected = match tcp.peek(&mut byte) {
                    Ok(0) => true,
                    Ok(_) => false,
                    Err(error) => !matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ),
                };
                let _ = tcp.set_nonblocking(false);
                disconnected
            }
        }
    }
}

impl Read for TransportStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(stream) => stream.read(buf),
            Self::LoopbackTcp(stream) => stream.read(buf),
        }
    }
}

impl Write for TransportStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Local(stream) => stream.write(buf),
            Self::LoopbackTcp(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Local(stream) => stream.flush(),
            Self::LoopbackTcp(stream) => stream.flush(),
        }
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for TransportStream {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        match self {
            Self::Local(stream) => stream.as_raw_fd(),
            Self::LoopbackTcp(stream) => stream.as_raw_fd(),
        }
    }
}

/// The primary local listener type used by the daemon's accept loop.
///
/// On Unix this wraps `std::os::unix::net::UnixListener`; on Windows it wraps
/// `WindowsNamedPipeListener`. Both provide `accept()` (returning
/// `(TransportStream, ())`) and `set_nonblocking(bool)`.
pub(crate) struct TransportListener {
    #[cfg(unix)]
    inner: std::os::unix::net::UnixListener,
    #[cfg(windows)]
    inner: WindowsNamedPipeListener,
}

impl TransportListener {
    pub(crate) fn accept(&self) -> std::io::Result<(TransportStream, ())> {
        let (stream, _) = self.inner.accept()?;
        Ok((TransportStream::Local(stream), ()))
    }

    pub(crate) fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.inner.set_nonblocking(nonblocking)
    }
}

/// Connect to the transport endpoint at `path`.
///
/// On Unix, `path` is a filesystem socket path (`UnixStream::connect`).
/// On Windows, a named-pipe name is derived from `path` and opened via
/// `CreateFileW`.
pub(crate) fn transport_connect(path: &Path) -> std::io::Result<TransportStream> {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).map(TransportStream::Local)
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeStream::connect(path).map(TransportStream::Local)
    }
}

/// Connect to a loopback TCP endpoint (`127.0.0.1:<port>`). Refuses non-loopback
/// addresses so client code can never accidentally dial a public network socket
/// directly without an SSH/SSM tunnel.
pub(crate) fn transport_connect_loopback(
    addr: std::net::SocketAddr,
) -> std::io::Result<TransportStream> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("refusing non-loopback IPC address: {addr}"),
        ));
    }
    let stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let _ = stream.set_nodelay(true);
    Ok(TransportStream::LoopbackTcp(stream))
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
        // The socket must never exist with a permissive mode, even between
        // bind and the owner-only chmod that follows: create it under an
        // owner-only umask (S12 of the 2026-09-20 review).
        // SAFETY: umask has no preconditions; the previous mask is restored
        // before returning on every path.
        let previous = unsafe { libc::umask(0o077) };
        let bound = std::os::unix::net::UnixListener::bind(path);
        unsafe { libc::umask(previous) };
        bound.map(|inner| TransportListener { inner })
    }
    #[cfg(windows)]
    {
        WindowsNamedPipeListener::bind(path).map(|inner| TransportListener { inner })
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
    // Deterministic 64-bit FNV-1a hash (same algorithm as `workspace_key`) so
    // a daemon and client built with different Rust toolchains always derive
    // the exact same named-pipe path.
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in path.as_os_str().as_encoded_bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
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
        safe_sid, hash
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
