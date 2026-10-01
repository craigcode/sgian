use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE,
    ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PeekNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, ResetEvent, WaitForSingleObject, INFINITE,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

// Pipe access mode and generic access rights constants. These are stable
// Win32 values; we define them locally because windows-sys 0.61 may not
// expose them under the enabled feature set.
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const PIPE_ACCESS_DUPLEX: u32 = 0x0000_0003;
/// Total time a client waits for the listener to accept the currently
/// connected pipe and create the next instance. A named-pipe server owns one
/// listening instance at a time, so concurrent clients legitimately see
/// ERROR_PIPE_BUSY during that short hand-off window.
const PIPE_CONNECT_WAIT: Duration = Duration::from_secs(2);

/// Derive a per-user Windows named-pipe path
/// (`\\.\pipe\<stable-namespace>-<user-sid>-<hash>`) from a filesystem
/// socket path.
/// Named pipes are kernel objects, not files; the path is hashed for a
/// valid name and the current user's SID string is folded in so the pipe is
/// scoped to one user (two users cannot collide on, or connect to, the same
/// pipe). Fails CLOSED if the per-user SID cannot be resolved — there is no
/// fixed/placeholder fallback, so a derived name ALWAYS embeds a real SID.
pub(super) fn pipe_name_from_path(path: &Path) -> io::Result<String> {
    crate::pipe_name_from_sid(current_user_sid_string(), path)
}

/// Look up the current process user's SID as its SDDL string form (e.g.
/// `"S-1-5-21-..."`). Used both to scope the pipe name per user and to build
/// the owner-restricted pipe security descriptor. Returns `None` on any
/// failure so callers can fail CLOSED (refuse to derive a name / build a
/// descriptor) rather than fall back to insecure defaults.
fn current_user_sid_string() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = INVALID_HANDLE_VALUE;
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        // Size the TOKEN_USER buffer, then read it.
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let got = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if got == 0 {
            return None;
        }
        let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut sid_str: windows_sys::core::PWSTR = std::ptr::null_mut();
        if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str) == 0 || sid_str.is_null() {
            return None;
        }
        let mut len = 0usize;
        while *sid_str.add(len) != 0 {
            len += 1;
        }
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len));
        LocalFree(sid_str as *mut core::ffi::c_void);
        Some(s)
    }
}

/// Build a self-relative security descriptor that grants full access only to
/// the current user's SID (a protected DACL, so no inherited ACEs widen it)
/// and labels the object at medium integrity. Returned pointer is
/// `LocalAlloc`-backed and must be freed with `free_security_descriptor`.
/// Returns NULL on any failure; callers MUST then fail CLOSED (refuse to
/// create/listen on the pipe) rather than fall back to default-ACL security.
fn build_owner_security_descriptor() -> windows_sys::Win32::Security::PSECURITY_DESCRIPTOR {
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::PSECURITY_DESCRIPTOR;

    let sid = match current_user_sid_string() {
        Some(sid) => sid,
        None => return std::ptr::null_mut(),
    };
    // D:P            -> protected DACL (ignore inheritable ACEs)
    // (A;;GA;;;<sid>)-> allow GENERIC_ALL to the owner SID only
    // S:(ML;;NW;;;ME)-> mandatory label: medium integrity, no-write-up
    let sddl = format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)");
    let wide_sddl = wide(&sddl);
    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut psd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        std::ptr::null_mut()
    } else {
        psd
    }
}

/// Free a security descriptor produced by `build_owner_security_descriptor`.
fn free_security_descriptor(sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR) {
    if !sd.is_null() {
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(sd as windows_sys::Win32::Foundation::HLOCAL);
        }
    }
}

/// Create one named-pipe server instance secured by the owner-restricted
/// descriptor. Fails CLOSED: a NULL descriptor returns `INVALID_HANDLE_VALUE`
/// WITHOUT calling `CreateNamedPipeW`, so the pipe is NEVER created with NULL
/// `SECURITY_ATTRIBUTES` (default/inherited ACLs). This is the single choke
/// point for both the initial bind and each post-accept recreate.
fn create_pipe_instance(
    wide_name: &[u16],
    sd: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
) -> HANDLE {
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    if crate::require_owner_descriptor(sd.is_null()).is_err() {
        return INVALID_HANDLE_VALUE;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            // UNLIMITED, not 1: accept hands the connected instance to the
            // returned stream (still OPEN) and only then recreates the next
            // listening instance — with nMaxInstances = 1 that recreate
            // always fails, orphaning the listener (first Windows smoke:
            // listener died with ERROR_INVALID_HANDLE on the next accept).
            // Live instances = accepted connections + 1 listener instance.
            PIPE_UNLIMITED_INSTANCES,
            4096, // out buffer
            4096, // in buffer
            0,    // default timeout
            &sa,
        )
    }
}

/// Convert a Rust string to a NUL-terminated UTF-16 vector for FFI.
fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Per-direction overlapped I/O state shared by a pipe stream and all its
/// `try_clone` duplicates.
///
/// Duplicated handles refer to the SAME kernel pipe object, so every clone
/// shares ONE OVERLAPPED + manual-reset event per direction, and the
/// enclosing Mutex serializes same-direction I/O across clones (the kernel
/// writes the OVERLAPPED asynchronously, so two in-flight ops must never
/// share one). Reads and writes use SEPARATE state so a parked read never
/// blocks a write on another clone — e.g. the subscriber watcher thread
/// reading (unbounded) while the writer thread writes — matching
/// `UnixStream` clone semantics (H5).
struct PipeIo {
    /// Manual-reset completion event (also `overlapped.hEvent`). Owned
    /// here; closed on drop.
    event: HANDLE,
    overlapped: OVERLAPPED,
}

impl PipeIo {
    fn new() -> io::Result<Self> {
        // SAFETY: bManualReset = TRUE (the event is reused across ops and
        // must stay signaled until observed), initially nonsignaled so no
        // wait can complete spuriously; unnamed, default security. Returns
        // a valid handle or NULL.
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event;
        Ok(Self { event, overlapped })
    }
}

impl Drop for PipeIo {
    fn drop(&mut self) {
        // SAFETY: `event` was created in `PipeIo::new` and is closed exactly
        // once here. No I/O can be in flight: an in-flight transfer borrows
        // its stream (keeping this shared state alive), so drop only runs
        // after every op has been drained.
        unsafe {
            CloseHandle(self.event);
        }
    }
}

/// Sentinel for "no deadline" in the atomic timeout slots (an
/// `Option<Duration>` is not atomic; finite durations saturate below this).
const TIMEOUT_NEVER_MS: u64 = u64::MAX;

/// Shared per-CONNECTION state for a connected pipe and all its `try_clone`
/// duplicates (H5).
struct PipeShared {
    read: Mutex<PipeIo>,
    write: Mutex<PipeIo>,
    /// Read/write deadlines in milliseconds (`TIMEOUT_NEVER_MS` = wait
    /// forever). Atomics — NOT under the direction mutexes — so
    /// `set_*_timeout` never blocks behind an in-flight op on a clone.
    read_timeout_ms: AtomicU64,
    write_timeout_ms: AtomicU64,
}

impl PipeShared {
    fn new() -> io::Result<Arc<Self>> {
        // If the second event fails, the first `PipeIo` drops here and
        // closes its own event, so nothing leaks.
        let read = PipeIo::new()?;
        let write = PipeIo::new()?;
        Ok(Arc::new(Self {
            read: Mutex::new(read),
            write: Mutex::new(write),
            read_timeout_ms: AtomicU64::new(TIMEOUT_NEVER_MS),
            write_timeout_ms: AtomicU64::new(TIMEOUT_NEVER_MS),
        }))
    }
}

/// Encode a deadline for the atomic slots, rounding UP to the next
/// millisecond so a sub-ms duration still bounds the wait (std rejects
/// zero timeouts outright; rounding up is the closest pipe analog — a
/// stored 0 becomes an immediate poll).
fn encode_timeout_ms(dur: Option<Duration>) -> u64 {
    match dur {
        None => TIMEOUT_NEVER_MS,
        Some(dur) => {
            let ms = dur
                .as_millis()
                .saturating_add(u128::from(dur.subsec_nanos() % 1_000_000 != 0));
            u64::try_from(ms).unwrap_or(TIMEOUT_NEVER_MS - 1)
        }
    }
}

/// Map a stored deadline to a Win32 wait timeout, keeping finite values
/// below `INFINITE` so a huge deadline cannot alias to an infinite wait.
fn win_wait_ms(ms: u64) -> u32 {
    if ms == TIMEOUT_NEVER_MS {
        INFINITE
    } else {
        u32::try_from(ms).unwrap_or(INFINITE - 1)
    }
}

/// Run one overlapped transfer (`op` = ReadFile/WriteFile) to completion,
/// bounding the completion wait by `timeout_ms` (`TIMEOUT_NEVER_MS` =
/// infinite). The direction mutex is held for the WHOLE operation
/// (issue → wait → collect), serializing same-direction I/O across every
/// clone sharing this `PipeIo`; the OVERLAPPED is reused only after its
/// previous op was fully drained here (H5: these pipe handles are created
/// with FILE_FLAG_OVERLAPPED, where a NULL `lpOverlapped` is documented to
/// fail/misbehave).
///
/// `op` receives the handle, the shared OVERLAPPED, and the immediate-byte
/// out-param, and returns the raw Win32 BOOL.
fn overlapped_transfer(
    io: &Mutex<PipeIo>,
    handle: HANDLE,
    timeout_ms: u64,
    op: impl FnOnce(HANDLE, *mut OVERLAPPED, *mut u32) -> i32,
) -> io::Result<usize> {
    // A poisoned lock means a thread panicked mid-transfer; propagating the
    // panic beats risking reuse of a possibly-still-pending OVERLAPPED.
    let mut io = io.lock().expect("pipe I/O mutex poisoned");
    // SAFETY: `io.event` is a live event owned by this PipeIo. Reset BEFORE
    // issuing so a signal left by a previous op cannot complete the wait
    // below spuriously (the event is manual-reset and reused across ops).
    unsafe {
        ResetEvent(io.event);
    }
    io.overlapped.Internal = 0;
    io.overlapped.InternalHigh = 0;
    let mut transferred: u32 = 0;
    let overlapped_ptr: *mut OVERLAPPED = &mut io.overlapped;
    let immediate = op(handle, overlapped_ptr, &mut transferred);
    if immediate != 0 {
        // Completed synchronously: `transferred` is already valid, no wait.
        return Ok(transferred as usize);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
        return Err(err);
    }
    // In flight: wait for the completion event, bounded by the deadline.
    // SAFETY: the event is valid and the OVERLAPPED stays valid and
    // un-reused for the whole wait (this critical section outlives it).
    let wait = unsafe { WaitForSingleObject(io.event, win_wait_ms(timeout_ms)) };
    match wait {
        WAIT_OBJECT_0 => {
            let mut done: u32 = 0;
            // SAFETY: the op completed (event signaled); bWait = FALSE only
            // collects the finished op's result.
            let ok = unsafe { GetOverlappedResult(handle, overlapped_ptr, &mut done, 0) };
            if ok == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(done as usize)
            }
        }
        WAIT_TIMEOUT => {
            // Deadline expired: cancel, then DRAIN — cancellation is
            // asynchronous, so the kernel may still touch the OVERLAPPED
            // after CancelIoEx returns. GetOverlappedResult(bWait = TRUE)
            // blocks until the canceled op is fully complete, after which
            // the OVERLAPPED is safe to reuse.
            // SAFETY: handle/OVERLAPPED valid as above.
            unsafe {
                CancelIoEx(handle, overlapped_ptr);
                let mut done: u32 = 0;
                GetOverlappedResult(handle, overlapped_ptr, &mut done, 1);
            }
            // Match std UnixStream semantics: an expired read/write
            // deadline reports WouldBlock, which callers were written
            // against (e.g. the M6 quiet-but-alive liveness check).
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "named-pipe I/O deadline expired",
            ))
        }
        // WAIT_FAILED (or anything unexpected): surface the OS error.
        _ => Err(io::Error::last_os_error()),
    }
}

/// A connected named-pipe stream (client or accepted server side).
///
/// Each stream owns one HANDLE (a `try_clone` duplicate shares the same
/// kernel pipe object); the OVERLAPPED/event/deadline state is
/// per-CONNECTION and shared via `Arc`, so clones coordinate their I/O (H5).
pub struct WindowsNamedPipeStream {
    handle: HANDLE,
    shared: Arc<PipeShared>,
}

// SAFETY: Windows HANDLEs are process-global opaque pointers. Each stream's
// handle is immutable after construction and is closed exactly once by its
// own `Drop`; all shared mutable state (the two OVERLAPPED structs and
// their completion events) is behind the `PipeShared` mutexes and the
// deadlines are atomic, so no `&self` method can race. `Send` is required
// so the stream can be moved into the per-connection and subscriber
// writer/watcher threads, matching `UnixStream: Send + Sync`.
unsafe impl Send for WindowsNamedPipeStream {}
unsafe impl Sync for WindowsNamedPipeStream {}

impl std::fmt::Debug for WindowsNamedPipeStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WindowsNamedPipeStream")
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl WindowsNamedPipeStream {
    /// Connect to a named-pipe server whose name is derived from `path`.
    pub fn connect(path: &Path) -> io::Result<Self> {
        let name = pipe_name_from_path(path)?;
        let wide_name = wide(&name);
        // FILE_FLAG_OVERLAPPED: stream I/O goes through the shared-state
        // overlapped path so set_read_timeout/set_write_timeout actually
        // bound it — a synchronous handle would block INSIDE the kernel
        // call, unreachable by any deadline (H5: this guts
        // CLIENT_READ_TIMEOUT on the client side otherwise).
        let started = std::time::Instant::now();
        let handle = loop {
            let handle = unsafe {
                CreateFileW(
                    wide_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_NONE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    std::ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                break handle;
            }

            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
                return Err(error);
            }

            // ERROR_PIPE_BUSY does not mean the daemon is wedged. It means
            // every instance that exists *right now* is connected; with our
            // one-at-a-time listener this is the normal concurrent-connect
            // race. Microsoft documents WaitNamedPipe as the required client
            // recovery before retrying CreateFile.
            let remaining = PIPE_CONNECT_WAIT.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for a named-pipe instance",
                ));
            }
            let wait_ms = u32::try_from(remaining.as_millis().max(1))
                .unwrap_or(u32::MAX - 1)
                .min(u32::MAX - 1);
            if unsafe { WaitNamedPipeW(wide_name.as_ptr(), wait_ms) } == 0 {
                let wait_error = io::Error::last_os_error();
                if started.elapsed() >= PIPE_CONNECT_WAIT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting for a named-pipe instance: {wait_error}"),
                    ));
                }
                return Err(wait_error);
            }
        };
        match PipeShared::new() {
            Ok(shared) => Ok(Self { handle, shared }),
            Err(error) => {
                // SAFETY: `handle` is a live pipe handle owned by this
                // function; close it so a failed event setup can't leak it.
                unsafe {
                    CloseHandle(handle);
                }
                Err(error)
            }
        }
    }

    /// Create a stream from an existing pipe handle (used by the listener
    /// after `ConnectNamedPipe` succeeds). Takes ownership of `handle` in
    /// ALL cases: on failure the handle is closed here rather than leaked.
    fn from_handle(handle: HANDLE) -> io::Result<Self> {
        match PipeShared::new() {
            Ok(shared) => Ok(Self { handle, shared }),
            Err(error) => {
                // SAFETY: `handle` is a live pipe handle owned by this
                // function; the caller propagates the error and never sees
                // a stream, so close the handle here (exactly once).
                unsafe {
                    CloseHandle(handle);
                }
                Err(error)
            }
        }
    }

    /// Duplicate the underlying handle (equivalent to `UnixStream::try_clone`).
    /// The clone shares the connection's overlapped state/deadlines via `Arc`.
    pub fn try_clone(&self) -> io::Result<Self> {
        let mut dup_handle: HANDLE = INVALID_HANDLE_VALUE;
        // SAFETY: both process handles are this process, `self.handle` is a
        // live pipe handle, and `dup_handle` is a valid out-pointer.
        // bInheritHandle = FALSE (H5): a duplicated IPC handle must NOT be
        // inheritable — every PTY child spawns with handle inheritance
        // enabled, so an inheritable clone would leak into spawned shells.
        let ok = unsafe {
            DuplicateHandle(
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                self.handle,
                windows_sys::Win32::System::Threading::GetCurrentProcess(),
                &mut dup_handle,
                0,
                0, // bInheritHandle = FALSE (H5)
                DUPLICATE_SAME_ACCESS,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            handle: dup_handle,
            shared: Arc::clone(&self.shared),
        })
    }

    /// Shut down the pipe (best-effort flush + cancel pending I/O).
    /// Equivalent to `UnixStream::shutdown(Shutdown::Both)`.
    pub fn shutdown(&self, _how: std::net::Shutdown) -> io::Result<()> {
        // SAFETY: `self.handle` is a live pipe handle owned by this stream.
        // CancelIoEx(handle, NULL) cancels ALL outstanding overlapped I/O
        // on the pipe object (any issuing thread), so a reader/writer
        // parked on a clone wakes with an error — the shutdown(2) wake-up
        // analog. FlushFileBuffers is best-effort; ignore failure on a
        // broken pipe.
        unsafe {
            let _ = CancelIoEx(self.handle, std::ptr::null());
            let _ = FlushFileBuffers(self.handle);
        }
        Ok(())
    }

    /// Set the write deadline: bounds the completion wait of every
    /// subsequent `write` (overlapped I/O, so the wait is interruptible).
    /// `None` waits forever. Shared by all clones, like `UnixStream`.
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.shared
            .write_timeout_ms
            .store(encode_timeout_ms(dur), Ordering::Relaxed);
        Ok(())
    }

    /// Set the read deadline (same semantics as the write deadline).
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> io::Result<()> {
        self.shared
            .read_timeout_ms
            .store(encode_timeout_ms(dur), Ordering::Relaxed);
        Ok(())
    }

    /// Toggle non-blocking mode. Named pipes use overlapped I/O for
    /// non-blocking; this is a best-effort no-op since the daemon only
    /// calls `set_nonblocking(false)` after accepting.
    pub fn set_nonblocking(&self, _nonblocking: bool) -> io::Result<()> {
        Ok(())
    }

    /// Non-consuming peer-liveness probe for timeout-less wait handlers.
    /// PeekNamedPipe succeeds for a quiet live peer (zero bytes available)
    /// and fails once the client handle has closed, so abandoned waits can
    /// release their connection/pipe instance just as Unix MSG_PEEK does.
    pub fn is_peer_disconnected(&self) -> bool {
        unsafe {
            PeekNamedPipe(
                self.handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ) == 0
        }
    }
}

impl Read for WindowsNamedPipeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let timeout_ms = self.shared.read_timeout_ms.load(Ordering::Relaxed);
        let result = overlapped_transfer(
            &self.shared.read,
            self.handle,
            timeout_ms,
            |handle, overlapped, transferred| {
                // SAFETY: `buf` is a live exclusive slice for the whole
                // call, and `overlapped_transfer` fully drains the op
                // (completion, or cancel + drain) before returning, so the
                // kernel never touches `buf` after `read` returns.
                unsafe {
                    ReadFile(
                        handle,
                        buf.as_mut_ptr() as *mut _,
                        buf.len() as u32,
                        transferred,
                        overlapped,
                    )
                }
            },
        );
        match result {
            // ERROR_BROKEN_PIPE on a read is the named-pipe EOF: the peer
            // closed its end. Report it as an orderly EOF (Unix read == 0)
            // so frame/line readers see a clean close, not an error.
            Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => Ok(0),
            other => other,
        }
    }
}

impl Write for WindowsNamedPipeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let timeout_ms = self.shared.write_timeout_ms.load(Ordering::Relaxed);
        overlapped_transfer(
            &self.shared.write,
            self.handle,
            timeout_ms,
            |handle, overlapped, transferred| {
                // SAFETY: `buf` is a live shared slice for the whole call;
                // the op is fully drained before returning (see `read`).
                unsafe {
                    WriteFile(
                        handle,
                        buf.as_ptr() as *const _,
                        buf.len() as u32,
                        transferred,
                        overlapped,
                    )
                }
            },
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        // SAFETY: `self.handle` is a live pipe handle owned by this stream;
        // FlushFileBuffers is synchronous and touches no OVERLAPPED state.
        unsafe {
            if FlushFileBuffers(self.handle) == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl Drop for WindowsNamedPipeStream {
    fn drop(&mut self) {
        // SAFETY: each clone closes ONLY its own duplicated handle; the
        // kernel pipe object dies with the last close, and the shared
        // events are closed by `PipeIo::drop` once the last Arc is gone.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// A named-pipe listener (server side). Creates pipe instances with
/// `CreateNamedPipeW` and accepts connections via `ConnectNamedPipe`.
/// Non-blocking accept uses overlapped I/O to return `WouldBlock`.
///
/// Methods take `&self` (matching `UnixListener::accept(&self)`) via
/// interior mutability: `Cell` for the handle and boolean flags,
/// `UnsafeCell` for the overlapped struct (accessed via raw pointers
/// for FFI, same as the FFI calls themselves).
pub struct WindowsNamedPipeListener {
    handle: std::cell::Cell<HANDLE>,
    pipe_name: String,
    nonblocking: std::cell::Cell<bool>,
    overlapped: std::cell::UnsafeCell<OVERLAPPED>,
    connect_pending: std::cell::Cell<bool>,
    /// Set when `ConnectNamedPipe` established the connection SYNCHRONOUSLY
    /// (an immediate nonzero return, or ERROR_PIPE_CONNECTED — a client
    /// connected between CreateNamedPipeW and ConnectNamedPipe, which per
    /// Win32 semantics is a SUCCESSFUL connection). No overlapped operation
    /// is pending in this state, so `accept` must skip `GetOverlappedResult`
    /// (waiting on it could block forever; H5).
    connected_ready: std::cell::Cell<bool>,
    /// Owner-restricted security descriptor reused for every pipe instance
    /// this listener creates (initial bind + each post-accept recreate).
    /// Freed on drop. Guaranteed non-NULL: `bind` fails closed before
    /// constructing the listener if the descriptor cannot be built, so a
    /// post-accept recreate never downgrades to default-ACL security.
    security_descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
}

// SAFETY: the listener is not Sync and can only be used by the one thread
// that owns it after a move. Win32 handles, OVERLAPPED storage, and the
// LocalAlloc security descriptor are process-wide resources that may be
// used/freed from a different thread. No references into the listener are
// retained across the move.
unsafe impl Send for WindowsNamedPipeListener {}

impl WindowsNamedPipeListener {
    /// Create a named-pipe server whose name is derived from `path`, secured
    /// to the owner SID. Fails CLOSED: if the per-user SID cannot be resolved
    /// or the owner-restricted security descriptor cannot be built, the bind is
    /// refused rather than listening on an unscoped / default-ACL pipe.
    pub fn bind(path: &Path) -> io::Result<Self> {
        let pipe_name = pipe_name_from_path(path)?;
        let wide_name = wide(&pipe_name);
        let security_descriptor = build_owner_security_descriptor();
        // Fail closed: never listen on a pipe without an owner-restricted
        // descriptor (no default-ACL fallback). A NULL descriptor is also
        // refused at `create_pipe_instance`, but this surfaces a clear error
        // instead of a stale `last_os_error()`.
        crate::require_owner_descriptor(security_descriptor.is_null())?;
        let handle = create_pipe_instance(&wide_name, security_descriptor);
        if handle == INVALID_HANDLE_VALUE {
            free_security_descriptor(security_descriptor);
            return Err(io::Error::last_os_error());
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            unsafe {
                CloseHandle(handle);
            }
            free_security_descriptor(security_descriptor);
            return Err(io::Error::last_os_error());
        }
        let mut overlapped = OVERLAPPED::default();
        overlapped.hEvent = event;
        Ok(Self {
            handle: std::cell::Cell::new(handle),
            pipe_name,
            nonblocking: std::cell::Cell::new(false),
            overlapped: std::cell::UnsafeCell::new(overlapped),
            connect_pending: std::cell::Cell::new(false),
            connected_ready: std::cell::Cell::new(false),
            security_descriptor,
        })
    }

    /// Start an overlapped `ConnectNamedPipe` if one is not already pending
    /// (or already established synchronously).
    fn start_connect(&self) -> io::Result<()> {
        if self.connect_pending.get() || self.connected_ready.get() {
            return Ok(());
        }
        let result = unsafe { ConnectNamedPipe(self.handle.get(), self.overlapped.get()) };
        if result != 0 {
            // Connected SYNCHRONOUSLY (rare): no overlapped operation was
            // started, so there is nothing for GetOverlappedResult to
            // collect — record the ready connection directly.
            self.connected_ready.set(true);
            return Ok(());
        }
        let err = io::Error::last_os_error();
        // ERROR_IO_PENDING (997) — the overlapped connect is in progress; not
        // an error. (A previous revision compared against 535, which is
        // ERROR_PIPE_CONNECTED — every normal pending connect was then treated
        // as a fatal listener error; H5.)
        if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
            self.connect_pending.set(true);
            return Ok(());
        }
        // ERROR_PIPE_CONNECTED (535) — a client connected between
        // CreateNamedPipeW and ConnectNamedPipe. Per Win32 semantics this is
        // a SUCCESSFUL connection: the pipe IS connected and no wait is
        // needed. Propagating it as an error killed the daemon via the
        // accept loop's catch-all (H5).
        if err.raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32) {
            self.connected_ready.set(true);
            return Ok(());
        }
        Err(err)
    }

    /// Accept a client connection. In non-blocking mode, returns
    /// `WouldBlock` when no client is pending.
    pub fn accept(&self) -> io::Result<(WindowsNamedPipeStream, ())> {
        if !self.connect_pending.get() && !self.connected_ready.get() {
            self.start_connect()?;
        }

        if self.connect_pending.get() {
            let mut bytes_transferred: u32 = 0;
            let ok = unsafe {
                GetOverlappedResult(
                    self.handle.get(),
                    self.overlapped.get(),
                    &mut bytes_transferred,
                    if self.nonblocking.get() { 0 } else { 1 },
                )
            };
            if ok == 0 {
                let err = io::Error::last_os_error();
                // ERROR_IO_INCOMPLETE (996) — still pending in non-blocking mode.
                // (Previously compared against 534 = ERROR_ARITHMETIC_OVERFLOW, so
                // the WouldBlock mapping never fired; H5.)
                if self.nonblocking.get() && err.raw_os_error() == Some(ERROR_IO_INCOMPLETE as i32)
                {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, err));
                }
                return Err(err);
            }
            self.connect_pending.set(false);
        } else {
            // The connection completed synchronously (immediate return or
            // ERROR_PIPE_CONNECTED): the pipe is already connected and no
            // overlapped operation is pending, so there is no result to
            // collect — just consume the ready flag.
            self.connected_ready.set(false);
        }

        // Client connected: hand off the current pipe handle (the returned
        // stream OWNS it and closes it on drop) and create a new instance for
        // the next client. On any recreate failure the listener's handle is
        // set to INVALID_HANDLE_VALUE — never back to `connected`, which
        // would leave the same HANDLE owned by both the stream and the
        // listener's Drop (double CloseHandle, use-after-free class; H5).
        // The next accept then fails cleanly on the invalid handle.
        // `from_handle` takes ownership of `connected` in ALL cases (it
        // closes the handle itself if stream setup fails).
        let connected = self.handle.get();
        let wide_name = wide(&self.pipe_name);
        let new_handle = create_pipe_instance(&wide_name, self.security_descriptor);
        if new_handle == INVALID_HANDLE_VALUE {
            // We still have a connected client — return it; the listener is
            // left handle-less and errors on the next accept.
            // SAFETY: the old connect event is owned by the listener and is
            // replaced by OVERLAPPED::default() below, so this closes it
            // exactly once; `overlapped` is only mutated here and in `bind`.
            unsafe {
                CloseHandle((*self.overlapped.get()).hEvent);
            }
            self.handle.set(INVALID_HANDLE_VALUE);
            unsafe {
                *self.overlapped.get() = OVERLAPPED::default();
            }
            return WindowsNamedPipeStream::from_handle(connected).map(|stream| (stream, ()));
        }
        // Create a new event for the next overlapped connect.
        // SAFETY: manual-reset, initially nonsignaled, unnamed, default
        // security (same contract as `bind`'s event).
        let new_event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if new_event.is_null() {
            // SAFETY: `new_handle` is owned by this function and `connected`
            // is handed off below, so each handle is closed exactly once;
            // the old connect event is owned by the listener and replaced
            // with a zeroed OVERLAPPED, so it too is closed exactly once.
            unsafe {
                CloseHandle(new_handle);
                // Close the old overlapped event too (it was only replaced on
                // the success path) so it doesn't leak.
                CloseHandle((*self.overlapped.get()).hEvent);
            }
            self.handle.set(INVALID_HANDLE_VALUE);
            unsafe {
                *self.overlapped.get() = OVERLAPPED::default();
            }
            return WindowsNamedPipeStream::from_handle(connected).map(|stream| (stream, ()));
        }
        // Clean up the old overlapped event and set up the new one.
        // SAFETY: same single-close discipline as above — the old event is
        // closed once and the OVERLAPPED is replaced wholesale.
        unsafe {
            CloseHandle((*self.overlapped.get()).hEvent);
            *self.overlapped.get() = OVERLAPPED::default();
            (*self.overlapped.get()).hEvent = new_event;
        }
        self.handle.set(new_handle);
        WindowsNamedPipeStream::from_handle(connected).map(|stream| (stream, ()))
    }

    /// Test helper: leave the listener handle-less the way a failed
    /// post-accept `create_pipe_instance` does, so the next accept must
    /// fail cleanly instead of double-closing a handed-off HANDLE.
    #[cfg(test)]
    fn force_handleless_after_recreate_failure_for_test(&self) {
        self.handle.set(INVALID_HANDLE_VALUE);
        self.connect_pending.set(false);
        self.connected_ready.set(false);
    }

    /// Toggle non-blocking accept mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.nonblocking.set(nonblocking);
        Ok(())
    }
}

impl Drop for WindowsNamedPipeListener {
    fn drop(&mut self) {
        unsafe {
            // The handle is INVALID after a recreate-failure fallback (the
            // connected pipe's ownership moved to the returned stream) and the
            // event may be NULL after the same path; close only what we own.
            if self.handle.get() != INVALID_HANDLE_VALUE {
                CloseHandle(self.handle.get());
            }
            let event = (*self.overlapped.get()).hEvent;
            if !event.is_null() {
                CloseHandle(event);
            }
        }
        free_security_descriptor(self.security_descriptor);
    }
}

#[cfg(test)]
mod windows_pipe_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// VAL-IPC-037: the Windows pipe name is per-user (SID-scoped), not a
    /// bare path hash. Mirrors `pipe_name_from_path`'s composition exactly so
    /// it stays in lockstep with the live derivation.
    #[test]
    fn windows_pipe_name_is_sid_scoped() {
        let path = Path::new("C:/sgian/ws/daemon.sock");
        // A real Windows session resolves a SID, so the name builds; the
        // fail-closed branch (None ⇒ Err) is covered on any host by the
        // macOS-runnable `pipe_name_from_sid_fails_closed_without_sid` test.
        let name = pipe_name_from_path(path).expect("a Windows session resolves a SID");
        assert!(
            name.starts_with(&format!("\\\\.\\pipe\\{}-", crate::WINDOWS_IPC_NAMESPACE)),
            "unexpected pipe prefix: {name}"
        );
        let sid = current_user_sid_string().expect("SID available on Windows");
        assert_eq!(name, crate::pipe_name_with_sid(&sid, path));
        // The SID component must actually appear in the derived name.
        let safe_sid: String = sid
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        assert!(
            name.contains(&safe_sid),
            "pipe name {name} missing SID component {safe_sid}"
        );
    }

    /// VAL-IPC-038: the bind path constructs an owner-restricted security
    /// descriptor (or NULL fallback) — it must not panic and, when a SID is
    /// available, must produce a non-NULL descriptor that we can free.
    #[test]
    fn owner_security_descriptor_is_built_for_current_user() {
        let sd = build_owner_security_descriptor();
        if current_user_sid_string().is_some() {
            assert!(!sd.is_null(), "expected an owner-restricted descriptor");
        }
        free_security_descriptor(sd);
    }

    /// ma-scrutiny-fixes (2): the pipe-creation choke point fails CLOSED — a NULL
    /// security descriptor returns INVALID_HANDLE_VALUE and is never passed to
    /// CreateNamedPipeW as NULL SECURITY_ATTRIBUTES (no default-ACL fallback), on
    /// both the initial bind and each post-accept recreate. The Windows CI
    /// job compiles the complete test harness and runs this module natively.
    #[test]
    fn create_pipe_instance_refuses_null_descriptor() {
        let wide_name = wide(&format!(
            "\\\\.\\pipe\\{}-test-null-sd",
            crate::WINDOWS_IPC_NAMESPACE
        ));
        let handle = create_pipe_instance(&wide_name, std::ptr::null_mut());
        assert_eq!(
            handle, INVALID_HANDLE_VALUE,
            "a NULL descriptor must fail closed (no default-ACL pipe)"
        );
    }

    /// ENHANCEMENTS §5: after a post-accept recreate failure the listener
    /// is left handle-less; the next accept must fail with a bounded error
    /// (not hang or double-close the client HANDLE).
    #[test]
    fn listener_accept_fails_cleanly_when_left_handleless() {
        let path = std::env::temp_dir().join(format!(
            "sgian-pipe-handleless-{}-{}",
            std::process::id(),
            crate::now_millis()
        ));
        let listener = WindowsNamedPipeListener::bind(&path)
            .expect("bind should succeed with an owner descriptor");
        listener.force_handleless_after_recreate_failure_for_test();
        listener
            .set_nonblocking(true)
            .expect("nonblocking mode should apply");
        let err = listener.accept().expect_err("handle-less accept must fail");
        assert_ne!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "must not spin as WouldBlock forever on an invalid handle"
        );
    }

    /// Regression: CreateMutex reports ERROR_ALREADY_EXISTS even when an
    /// existing mutex object is currently UNOWNED. The daemon guard must
    /// test/acquire ownership rather than treating existence as contention,
    /// or an installed GUI can spawn a child that exits without a pipe.
    #[test]
    fn daemon_mutex_acquires_existing_unowned_object() {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let key = format!(
            "test-unowned-{}-{}",
            std::process::id(),
            crate::now_millis()
        );
        let name = format!("Local\\{}-daemon-{key}", crate::WINDOWS_IPC_NAMESPACE);
        let wide_name: Vec<u16> = std::ffi::OsStr::new(&name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let existing = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };
        assert!(!existing.is_null(), "create unowned mutex object");

        let guard = crate::acquire_windows_daemon_mutex(&key)
            .expect("mutex probe should succeed")
            .expect("an existing but unowned mutex must be acquired");
        drop(guard);
        unsafe { CloseHandle(existing) };
    }

    /// A mutex owned by another thread remains true contention. This pins
    /// the other half of the ownership probe while avoiding Windows mutexes'
    /// recursive same-thread acquisition semantics.
    #[test]
    fn daemon_mutex_defers_while_another_thread_owns_it() {
        let key = format!("test-owned-{}-{}", std::process::id(), crate::now_millis());
        let guard = crate::acquire_windows_daemon_mutex(&key)
            .expect("initial acquire should succeed")
            .expect("initial acquire should own the mutex");

        let key_for_thread = key.clone();
        let contender = std::thread::spawn(move || {
            crate::acquire_windows_daemon_mutex(&key_for_thread)
                .expect("contender probe should not error")
                .is_none()
        });
        assert!(
            contender.join().expect("contender thread should finish"),
            "a mutex owned by another thread must report contention"
        );
        drop(guard);
        assert!(
            crate::acquire_windows_daemon_mutex(&key)
                .expect("reacquire should succeed")
                .is_some(),
            "the mutex must become acquirable after owner release"
        );
    }

    /// Regression for the real installed-app failure: Windows resolves path
    /// components case-insensitively, and canonicalization must make the cwd
    /// collision guard accept differently-cased spellings of one directory.
    #[test]
    fn workspace_cwd_match_accepts_windows_case_variants() {
        let dir = tempfile::tempdir().expect("temp workspace root");
        let workspace = dir.path().join("SgianCaseWorkspace");
        std::fs::create_dir(&workspace).expect("create workspace");
        let case_variant = PathBuf::from(workspace.display().to_string().to_ascii_uppercase());
        assert!(
            crate::workspace_cwds_match(&workspace, &case_variant),
            "Windows case variants of one existing directory must match: {} vs {}",
            workspace.display(),
            case_variant.display()
        );
    }
}
