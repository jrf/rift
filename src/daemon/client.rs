//! Client-process side: takes the just-connected Unix socket from
//! `commands.rs`, puts the local terminal into raw mode, and proxies bytes
//! to/from the daemon via the tokio async stack.

use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use bytes::Bytes;
use nix::sys::signal::Signal;
use nix::sys::termios::{self, FlushArg, SetArg, Termios};
use nix::unistd;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::ipc::{self, RiftCodec, Tag};
use crate::socket;
use crate::util;

use super::ignore_signal;

/// Client-side output buffer cap. Above this, drop oldest bytes rather than
/// grow unbounded if stdout can't keep up.
const MAX_OUT_BUF: usize = 4 * 1024 * 1024;

/// Best-effort deadline for flushing already-buffered session output during
/// detach. Terminal mode restoration must not wait forever on a stalled stdout.
const FINAL_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const FINAL_DRAIN_RETRY: Duration = Duration::from_millis(1);

/// "Be sane" reset sent on attach and detach: disable all common mouse-tracking
/// variants (including 1016 SGR-pixel), focus reporting, bracketed paste;
/// exit alternate screen (1049 and the older 47); reset SGR; clear+home; show
/// cursor; exit alternate keypad. DECSTR (`\e[!p`), cursor-position-report and
/// scrolling-region reset were tried but triggered terminal status responses
/// that got echoed back to the user's shell — keep this set minimal. Kitty
/// keyboard flags are handled separately via push-at-attach / pop-at-detach
/// (`KBD_PUSH_RESET` / `KBD_POP`) — see `run_client`.
const TERMINAL_RESET: &[u8] = b"\
\x1b[?1000l\x1b[?1001l\x1b[?1002l\x1b[?1003l\x1b[?1004l\x1b[?1005l\x1b[?1006l\x1b[?1015l\x1b[?1016l\
\x1b[?2004l\
\x1b[?1049l\x1b[?47l\
\x1b[0m\
\x1b[2J\x1b[H\
\x1b[?25h\
\x1b>";

/// Push kitty keyboard flags = 0 onto the terminal's protocol stack. The OLD
/// current flags get preserved on the stack as a side effect of the push, so
/// `KBD_POP` at detach restores them exactly — even if the inner shell did
/// `CSI = u` SETs during the session (SET only overwrites *current*, never
/// touches the stack). On terminals that don't implement kitty kbd, this is
/// silently ignored.
const KBD_PUSH_RESET: &[u8] = b"\x1b[>0u";

/// Pop one entry from the kitty keyboard stack — restores the flags that were
/// current when we pushed at attach.
const KBD_POP: &[u8] = b"\x1b[<1u";

fn should_detach(data: &[u8], disabled: bool) -> bool {
    !disabled && (data.contains(&0x1c) || util::is_kitty_ctrl_backslash(data))
}

// ---------------------------------------------------------------------------
// Terminal raw mode
// ---------------------------------------------------------------------------

fn enter_raw_mode(fd: RawFd) -> io::Result<Termios> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let saved = termios::tcgetattr(bfd).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let mut raw = saved.clone();
    termios::cfmakeraw(&mut raw);
    raw.control_chars[nix::sys::termios::SpecialCharacterIndices::VQUIT as usize] = 0;
    termios::tcsetattr(bfd, SetArg::TCSAFLUSH, &raw)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(saved)
}

struct RawModeGuard {
    fd: RawFd,
    saved: Termios,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        // Restore with TCSANOW, not TCSAFLUSH: TCSAFLUSH discards pending
        // input, which over an SSH PTY (where bytes are often in flight)
        // can leave the terminal stuck in raw mode after detach. Also OR in
        // the must-have line-editing bits in case the saved state had them
        // disabled — a chained PTY (ssh inside ssh, rift inside tmux, etc.)
        // can capture a partially-disabled mode at attach time.
        use nix::sys::termios::{InputFlags, LocalFlags};
        let mut restored = self.saved.clone();
        restored.local_flags |= LocalFlags::ECHO
            | LocalFlags::ECHOE
            | LocalFlags::ECHOK
            | LocalFlags::ICANON
            | LocalFlags::ISIG
            | LocalFlags::IEXTEN;
        restored.input_flags |= InputFlags::ICRNL | InputFlags::BRKINT;
        let _ = termios::tcsetattr(bfd, SetArg::TCSANOW, &restored);
    }
}

/// Restores a borrowed descriptor's status and descriptor flags exactly as
/// they were before the client made it non-blocking and close-on-exec.
///
/// Construction is transactional: if either mutation fails, the already-made
/// changes are rolled back when the partially constructed guard is dropped.
struct FdFlagsGuard {
    fd: RawFd,
    status_flags: nix::fcntl::OFlag,
    descriptor_flags: nix::fcntl::FdFlag,
}

impl FdFlagsGuard {
    fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<Self> {
        use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};

        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let status_flags = fcntl(bfd, FcntlArg::F_GETFL)
            .map(OFlag::from_bits_truncate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        let descriptor_flags = fcntl(bfd, FcntlArg::F_GETFD)
            .map(FdFlag::from_bits_truncate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        let guard = Self {
            fd,
            status_flags,
            descriptor_flags,
        };

        fcntl(bfd, FcntlArg::F_SETFL(status_flags | OFlag::O_NONBLOCK))
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        fcntl(
            bfd,
            FcntlArg::F_SETFD(descriptor_flags | FdFlag::FD_CLOEXEC),
        )
        .map_err(|error| io::Error::from_raw_os_error(error as i32))?;

        Ok(guard)
    }
}

impl Drop for FdFlagsGuard {
    fn drop(&mut self) {
        use nix::fcntl::{FcntlArg, fcntl};

        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = fcntl(bfd, FcntlArg::F_SETFL(self.status_flags));
        let _ = fcntl(bfd, FcntlArg::F_SETFD(self.descriptor_flags));
    }
}

// ---------------------------------------------------------------------------
// Stdio fd wrapper for AsyncFd
// ---------------------------------------------------------------------------

/// No-close wrapper so `AsyncFd<StdioFd>` can register stdin/stdout with the
/// reactor without taking ownership of the fd. Dropping the wrapper does
/// NOT close the underlying fd — the OS still owns process stdio.
struct StdioFd(RawFd);
impl AsRawFd for StdioFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

// ---------------------------------------------------------------------------
// AsyncFd try_io helpers
// ---------------------------------------------------------------------------

/// Outcome of a single non-blocking read or write via `AsyncFd::try_io`.
enum IoStep {
    /// Read or wrote `n > 0` bytes.
    Bytes(usize),
    /// The fd reported ready but the operation would have blocked. Caller
    /// should re-await readiness (i.e. just continue the select loop).
    WouldBlock,
    /// EOF or unrecoverable error. Caller should stop.
    Closed,
}

/// Wrap the readiness-guard + `try_io` + nix-error conversion + outcome-match
/// pattern that otherwise repeats verbatim for every readable/writable branch.
fn try_read<T: AsRawFd>(
    ready: io::Result<tokio::io::unix::AsyncFdReadyGuard<'_, T>>,
    buf: &mut [u8],
) -> IoStep {
    let mut guard = match ready {
        Ok(g) => g,
        Err(_) => return IoStep::Closed,
    };
    let res = guard.try_io(|inner| {
        let bfd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
        unistd::read(bfd, buf).map_err(|e| io::Error::from_raw_os_error(e as i32))
    });
    match res {
        Ok(Ok(0)) | Ok(Err(_)) => IoStep::Closed,
        Ok(Ok(n)) => IoStep::Bytes(n),
        Err(_) => IoStep::WouldBlock,
    }
}

fn try_write<T: AsRawFd>(
    ready: io::Result<tokio::io::unix::AsyncFdReadyGuard<'_, T>>,
    buf: &[u8],
) -> IoStep {
    let mut guard = match ready {
        Ok(g) => g,
        Err(_) => return IoStep::Closed,
    };
    let res = guard.try_io(|inner| {
        let bfd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
        unistd::write(bfd, buf).map_err(|e| io::Error::from_raw_os_error(e as i32))
    });
    match res {
        Ok(Ok(0)) | Ok(Err(_)) => IoStep::Closed,
        Ok(Ok(n)) => IoStep::Bytes(n),
        Err(_) => IoStep::WouldBlock,
    }
}

// ---------------------------------------------------------------------------
// Client entry point
// ---------------------------------------------------------------------------

/// Run a client session against `socket`, driving raw-mode terminal I/O over
/// the daemon connection until the user detaches or the daemon hands us off to
/// another session. Returns `(exit_code, outcome)`.
pub fn run_client_outcome(socket: OwnedFd) -> (i32, ClientOutcome) {
    let socket_fd = socket.as_raw_fd();
    let stdin_fd: RawFd = 0;
    let stdout_fd: RawFd = 1;

    // The socket is owned by this function and will be closed on any failure,
    // so only borrowed stdio descriptors need restoration guards.
    if let Err(e) = socket::set_nonblock_and_cloexec(socket_fd) {
        eprintln!("error: failed to set socket nonblock: {}", e);
        return (1, ClientOutcome::Detached);
    }
    let _stdout_guard = match FdFlagsGuard::set_nonblock_and_cloexec(stdout_fd) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("error: failed to set stdout nonblock: {}", e);
            return (1, ClientOutcome::Detached);
        }
    };
    let _stdin_guard = match FdFlagsGuard::set_nonblock_and_cloexec(stdin_fd) {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("error: failed to set stdin nonblock: {}", e);
            return (1, ClientOutcome::Detached);
        }
    };

    let saved = match enter_raw_mode(stdin_fd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to enter raw mode: {}", e);
            return (1, ClientOutcome::Detached);
        }
    };
    let _raw_guard = RawModeGuard {
        fd: stdin_fd,
        saved,
    };

    // Sanitize the terminal before session bytes start arriving. On reattach,
    // the daemon will replay the full serialized state (Init), which paints
    // whatever modes the session actually needs — but it can't reliably
    // *unset* modes that were sticky on the local terminal (e.g. mouse
    // tracking left on by fzf), so we start from a known-clean baseline.
    write_terminal_reset(stdout_fd);

    write_bytes(stdout_fd, KBD_PUSH_RESET);

    ignore_signal(Signal::SIGPIPE);

    let std_socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(socket.into_raw_fd()) };
    if let Err(e) = std_socket.set_nonblocking(true) {
        eprintln!("error: failed to set socket nonblock: {}", e);
        return (1, ClientOutcome::Detached);
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: failed to build runtime: {}", e);
            return (1, ClientOutcome::Detached);
        }
    };

    let local = tokio::task::LocalSet::new();
    let outcome = local.block_on(&rt, async move {
        let stream = match UnixStream::from_std(std_socket) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: failed to wrap socket: {}", e);
                return ClientOutcome::Detached;
            }
        };
        client_async_main(stream, stdin_fd, stdout_fd).await
    });

    write_bytes(stdout_fd, KBD_POP);

    // Programs in the session (starship, vim, mouse-aware tools) may have
    // enabled DEC private modes that the detach path never gets to disable.
    // Send the standard "be sane" set before we restore termios so the
    // user's shell isn't stuck reporting mouse coords / hidden cursor.
    write_terminal_reset(stdout_fd);

    // Discard any bytes the terminal had queued on stdin at detach time —
    // typically trailing mouse coords, focus reports, or kitty kbd events
    // that the session had enabled but never got consumed by the select
    // loop. Without this they survive the TCSANOW restore below and land
    // as visible junk in the next program's stdin.
    let stdin_bfd = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
    let _ = termios::tcflush(stdin_bfd, FlushArg::TCIFLUSH);
    (0, outcome)
}

/// How a client run ended. `Detached` is the ordinary case (Ctrl+\, server
/// close, EOF). `Switch` means the daemon told us to hop to another session —
/// carrying the target name and the cwd to spawn it in if it doesn't exist.
pub enum ClientOutcome {
    Detached,
    Switch { name: String, cwd: Option<String> },
}

async fn client_async_main(stream: UnixStream, stdin_fd: RawFd, stdout_fd: RawFd) -> ClientOutcome {
    use futures_util::{SinkExt, StreamExt};

    let detach_key_disabled = std::env::var_os("RIFT_NO_DETACH_KEY").is_some();

    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, RiftCodec);
    let mut writer = FramedWrite::new(write_half, RiftCodec);

    let stdin_async = match AsyncFd::new(StdioFd(stdin_fd)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: failed to wrap stdin: {}", e);
            return ClientOutcome::Detached;
        }
    };
    let stdout_async = match AsyncFd::new(StdioFd(stdout_fd)) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: failed to wrap stdout: {}", e);
            return ClientOutcome::Detached;
        }
    };

    let mut sigwinch = match signal(SignalKind::window_change()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to register SIGWINCH: {}", e);
            return ClientOutcome::Detached;
        }
    };

    // Init explicitly marks this connection as an attached terminal. Other
    // commands may send Resize for headless sizing without becoming clients.
    let size = ipc::get_terminal_size(stdout_fd);
    let _ = writer
        .send((Tag::Init, Bytes::copy_from_slice(&size.encode())))
        .await;
    if let Ok(ssh_auth_sock) = std::env::var("SSH_AUTH_SOCK") {
        let _ = writer
            .send((
                Tag::SshAuthSock,
                Bytes::copy_from_slice(ssh_auth_sock.as_bytes()),
            ))
            .await;
    }
    // Send the tracked-environment snapshot so `rift print-env` can report the
    // environment this (leader) client is running under.
    let env_payload = crate::env::snapshot(&crate::env::tracked_keys());
    if !env_payload.is_empty() {
        let _ = writer
            .send((Tag::EnvSet, Bytes::copy_from_slice(env_payload.as_bytes())))
            .await;
    }

    let mut out_buf: Vec<u8> = Vec::new();
    let mut stdin_buf = [0u8; 4096];
    let mut outcome = ClientOutcome::Detached;

    loop {
        let has_pending = !out_buf.is_empty();

        tokio::select! {
            biased;

            _ = sigwinch.recv() => {
                let size = ipc::get_terminal_size(stdout_fd);
                let _ = writer
                    .send((Tag::Resize, Bytes::copy_from_slice(&size.encode())))
                    .await;
            }

            ready = stdin_async.readable() => {
                match try_read(ready, &mut stdin_buf) {
                    IoStep::Bytes(n) => {
                        let data = &stdin_buf[..n];
                        if should_detach(data, detach_key_disabled) {
                            let _ = writer.send((Tag::Detach, Bytes::new())).await;
                            break;
                        }
                        let _ = writer
                            .send((Tag::Input, Bytes::copy_from_slice(data)))
                            .await;
                    }
                    IoStep::Closed => break,
                    IoStep::WouldBlock => {}
                }
            }

            item = reader.next() => {
                let (tag, payload) = match item {
                    Some(Ok(frame)) => frame,
                    Some(Err(_)) | None => break,
                };
                let event = match ipc::DaemonEvent::decode(tag, payload) {
                    Ok(event) => event,
                    Err(_) => break,
                };
                match event {
                    ipc::DaemonEvent::Output(payload)
                    | ipc::DaemonEvent::TerminalState(payload) => {
                        if out_buf.len() + payload.len() > MAX_OUT_BUF {
                            let excess = out_buf.len() + payload.len() - MAX_OUT_BUF;
                            out_buf.drain(..excess.min(out_buf.len()));
                        }
                        out_buf.extend_from_slice(&payload);
                    }
                    ipc::DaemonEvent::Switch { name, cwd } => {
                        outcome = ClientOutcome::Switch { name, cwd };
                        break;
                    }
                    ipc::DaemonEvent::ResizeRequest => {
                        let size = ipc::get_terminal_size(stdout_fd);
                        let _ = writer
                            .send((Tag::Resize, Bytes::copy_from_slice(&size.encode())))
                            .await;
                    }
                    ipc::DaemonEvent::Detach => break,
                    _ => {}
                }
            }

            ready = stdout_async.writable(), if has_pending => {
                match try_write(ready, &out_buf) {
                    IoStep::Bytes(n) => { out_buf.drain(..n); }
                    IoStep::Closed => break,
                    IoStep::WouldBlock => {}
                }
            }
        }
    }

    // Best-effort final drain so tail bytes normally reach the terminal before
    // write_terminal_reset writes over them. A stalled stdout must not delay
    // terminal-mode restoration indefinitely.
    drain_bytes(stdout_fd, &mut out_buf, FINAL_DRAIN_TIMEOUT);

    outcome
}

fn write_terminal_reset(fd: RawFd) {
    write_bytes(fd, TERMINAL_RESET);
}

/// Drain mutable buffered output until it is empty, the descriptor fails, or
/// `timeout` expires. Stdout is nonblocking while the client runs, so bounding
/// EAGAIN retries keeps detach and raw-mode restoration reliable.
fn drain_bytes(fd: RawFd, bytes: &mut Vec<u8>, timeout: Duration) {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let deadline = Instant::now() + timeout;
    while !bytes.is_empty() && Instant::now() < deadline {
        match unistd::write(bfd, bytes) {
            Ok(n) if n > 0 => {
                bytes.drain(..n);
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) if Instant::now() < deadline => {
                std::thread::sleep(FINAL_DRAIN_RETRY);
            }
            _ => break,
        }
    }
}

/// Best-effort write of a fixed terminal-control sequence. These writes share
/// the final-drain bound so terminal cleanup cannot itself hang on full stdout.
fn write_bytes(fd: RawFd, bytes: &[u8]) {
    let mut pending = bytes.to_vec();
    drain_bytes(fd, &mut pending, FINAL_DRAIN_TIMEOUT);
}

#[cfg(test)]
mod tests {
    use super::{FdFlagsGuard, drain_bytes, should_detach};
    use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
    use nix::unistd::{pipe, write};
    use std::os::fd::{AsFd, AsRawFd};
    use std::time::{Duration, Instant};

    fn flags(fd: impl AsFd) -> (OFlag, FdFlag) {
        let fd = fd.as_fd();
        (
            OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).expect("status flags")),
            FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD).expect("descriptor flags")),
        )
    }

    #[test]
    fn fd_flags_guard_restores_exact_original_flags() {
        let (read_fd, _write_fd) = pipe().expect("pipe");
        let original = flags(&read_fd);

        {
            let _guard = FdFlagsGuard::set_nonblock_and_cloexec(read_fd.as_raw_fd())
                .expect("set nonblock and cloexec");
            let active = flags(&read_fd);
            assert!(active.0.contains(OFlag::O_NONBLOCK));
            assert!(active.1.contains(FdFlag::FD_CLOEXEC));
        }

        assert_eq!(flags(&read_fd), original);
    }

    #[test]
    fn fd_flags_guard_preserves_preexisting_nonblock() {
        let (read_fd, _write_fd) = pipe().expect("pipe");
        let fd = read_fd.as_fd();
        let original = flags(fd);
        fcntl(fd, FcntlArg::F_SETFL(original.0 | OFlag::O_NONBLOCK)).expect("set nonblock");
        fcntl(fd, FcntlArg::F_SETFD(original.1 | FdFlag::FD_CLOEXEC)).expect("set cloexec");
        let expected = flags(fd);

        {
            let _guard = FdFlagsGuard::set_nonblock_and_cloexec(read_fd.as_raw_fd())
                .expect("set nonblock and cloexec");
        }

        assert_eq!(flags(&read_fd), expected);
    }

    #[test]
    fn final_drain_stops_when_nonblocking_output_stays_full() {
        let (read_fd, write_fd) = pipe().expect("pipe");
        let flags =
            OFlag::from_bits_truncate(fcntl(&write_fd, FcntlArg::F_GETFL).expect("get pipe flags"));
        fcntl(&write_fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
            .expect("set pipe nonblocking");

        let fill = [0u8; 4096];
        while write(&write_fd, &fill).is_ok() {}

        let mut pending = b"terminal tail".to_vec();
        let started = Instant::now();
        drain_bytes(
            write_fd.as_raw_fd(),
            &mut pending,
            Duration::from_millis(20),
        );

        assert_eq!(pending, b"terminal tail");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "bounded drain took {:?}",
            started.elapsed()
        );
        drop(read_fd);
    }

    #[test]
    fn detach_key_can_be_disabled() {
        assert!(should_detach(&[0x1c], false));
        assert!(should_detach(b"\x1b[92;5u", false));
        assert!(!should_detach(&[0x1c], true));
        assert!(!should_detach(b"\x1b[92;5u", true));
    }
}
