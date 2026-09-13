//! Client-process side: takes the just-connected Unix socket from
//! `commands.rs`, puts the local terminal into raw mode, and proxies bytes
//! to/from the daemon via the tokio async stack.

use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

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

struct NonBlockGuard {
    fd: RawFd,
}

impl Drop for NonBlockGuard {
    fn drop(&mut self) {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        if let Ok(fl) = fcntl(bfd, FcntlArg::F_GETFL) {
            let fl = OFlag::from_bits_truncate(fl) & !OFlag::O_NONBLOCK;
            let _ = fcntl(bfd, FcntlArg::F_SETFL(fl));
        }
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

    for (fd, name) in [
        (socket_fd, "socket"),
        (stdout_fd, "stdout"),
        (stdin_fd, "stdin"),
    ] {
        if let Err(e) = socket::set_nonblock_and_cloexec(fd) {
            eprintln!("error: failed to set {} nonblock: {}", name, e);
            return (1, ClientOutcome::Detached);
        }
    }
    let _stdout_guard = NonBlockGuard { fd: stdout_fd };
    let _stdin_guard = NonBlockGuard { fd: stdin_fd };

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

    // Final synchronous drain so any tail bytes reach the terminal before
    // the runtime tears down (and write_terminal_reset writes over them).
    // Stdout is still O_NONBLOCK here; on EAGAIN we briefly back off and
    // retry rather than break — silently dropping the tail can chop an
    // escape sequence and leave its remainder visible as literal text.
    let bfd = unsafe { BorrowedFd::borrow_raw(stdout_fd) };
    while !out_buf.is_empty() {
        match unistd::write(bfd, &out_buf) {
            Ok(n) if n > 0 => {
                out_buf.drain(..n);
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            _ => break,
        }
    }

    outcome
}

fn write_terminal_reset(fd: RawFd) {
    write_bytes(fd, TERMINAL_RESET);
}

/// Blocking write of a fixed byte slice to `fd`, retrying on EAGAIN/EINTR.
/// Drains the kernel buffer so the bytes reach the terminal before we move
/// on (e.g. restore termios or exit) — otherwise they can be discarded.
fn write_bytes(fd: RawFd, bytes: &[u8]) {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut written = 0;
    while written < bytes.len() {
        match unistd::write(bfd, &bytes[written..]) {
            Ok(n) if n > 0 => written += n,
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EINTR) => continue,
            _ => break,
        }
    }
    let _ = termios::tcdrain(bfd);
}

#[cfg(test)]
mod tests {
    use super::should_detach;

    #[test]
    fn detach_key_can_be_disabled() {
        assert!(should_detach(&[0x1c], false));
        assert!(should_detach(b"\x1b[92;5u", false));
        assert!(!should_detach(&[0x1c], true));
        assert!(!should_detach(b"\x1b[92;5u", true));
    }
}
