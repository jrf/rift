//! Daemon-process side: owns the PTY, accepts client connections, drives
//! the vt100 parser, and brokers per-client tasks via mpsc channels.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::Signal;
use nix::unistd;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::ipc::{self, RiftCodec, Tag};
use crate::socket;
use crate::util;

use super::{ignore_signal, Cfg};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-client outgoing channel cap. Above this, the slow client is dropped
/// to prevent unbounded memory growth.
const CLIENT_TX_BUF: usize = 256;
const PTY_READ_BUF: usize = 4096;

// ---------------------------------------------------------------------------
// Low-level helpers (private to the server side)
// ---------------------------------------------------------------------------

fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    unistd::read(&bfd, buf)
}

fn redirect_std_to_devnull() {
    unsafe {
        let devnull = libc::open(b"/dev/null\0".as_ptr() as *const libc::c_char, libc::O_RDWR);
        if devnull >= 0 {
            libc::dup2(devnull, 0);
            libc::dup2(devnull, 1);
            libc::dup2(devnull, 2);
            if devnull > 2 {
                libc::close(devnull);
            }
        }
    }
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// PTY spawn + DA-query drain
// ---------------------------------------------------------------------------

fn drain_da_queries(master_fd: RawFd) -> Vec<u8> {
    let bfd = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
    let mut buf = [0u8; 4096];
    let mut collected = Vec::new();

    for _ in 0..200 {
        match poll(&mut poll_fds, PollTimeout::from(2u16)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }

        match read_raw(master_fd, &mut buf) {
            Ok(n) if n > 0 => {
                let data = &buf[..n];
                util::respond_to_device_attributes(master_fd, data);
                collected.extend_from_slice(data);
                return collected;
            }
            _ => continue,
        }
    }
    collected
}

fn spawn_pty(
    cmd: &str,
    args: &[&str],
    rows: u16,
    cols: u16,
    session_name: &str,
) -> io::Result<(RawFd, libc::pid_t)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_row = rows;
    ws.ws_col = cols;

    let mut master_fd: libc::c_int = -1;
    let pid = unsafe {
        libc::forkpty(
            &mut master_fd,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &ws as *const libc::winsize as *mut libc::winsize,
        )
    };

    if pid < 0 {
        return Err(io::Error::last_os_error());
    }

    if pid == 0 {
        unsafe {
            let key = std::ffi::CString::new("RIFT_SESSION").unwrap();
            let val = std::ffi::CString::new(session_name).unwrap();
            libc::setenv(key.as_ptr(), val.as_ptr(), 1);

            let sock_dir = socket::socket_dir();
            let symlink_path = sock_dir.join(format!("{}.ssh-auth-sock", session_name));
            if let Some(symlink_str) = symlink_path.to_str() {
                let key_ssh = std::ffi::CString::new("SSH_AUTH_SOCK").unwrap();
                let val_ssh = std::ffi::CString::new(symlink_str).unwrap();
                libc::setenv(key_ssh.as_ptr(), val_ssh.as_ptr(), 1);
            }

            libc::signal(libc::SIGPIPE, libc::SIG_DFL);

            if args.is_empty() {
                let shell_cstr = std::ffi::CString::new(cmd).unwrap();
                let login_name = format!("-{}", cmd.rsplit('/').next().unwrap_or(cmd));
                let login_cstr = std::ffi::CString::new(login_name).unwrap();
                libc::execl(
                    shell_cstr.as_ptr(),
                    login_cstr.as_ptr(),
                    std::ptr::null::<libc::c_char>(),
                );
            } else {
                let cmd_cstr = std::ffi::CString::new(cmd).unwrap();
                let mut argv: Vec<std::ffi::CString> = Vec::with_capacity(args.len() + 2);
                argv.push(std::ffi::CString::new(cmd.rsplit('/').next().unwrap_or(cmd)).unwrap());
                for arg in args {
                    argv.push(std::ffi::CString::new(*arg).unwrap());
                }
                let mut argv_ptrs: Vec<*const libc::c_char> =
                    argv.iter().map(|a| a.as_ptr()).collect();
                argv_ptrs.push(std::ptr::null());
                libc::execv(cmd_cstr.as_ptr(), argv_ptrs.as_ptr());
            }

            libc::_exit(127);
        }
    }

    socket::set_nonblock_and_cloexec(master_fd)?;
    Ok((master_fd, pid))
}

// ---------------------------------------------------------------------------
// Per-client task plumbing
// ---------------------------------------------------------------------------

/// Message from a client task back to the daemon main task.
enum ClientMsg {
    Frame { tag: Tag, payload: Vec<u8> },
    /// Read loop ended (socket EOF, error, or write task crashed).
    Gone,
}

/// Frame queued for delivery to a client task's socket.
#[derive(Clone)]
struct DaemonFrame {
    tag: Tag,
    payload: Bytes,
}

// ---------------------------------------------------------------------------
// DaemonState — owned exclusively by the main task
// ---------------------------------------------------------------------------

/// State owned exclusively by the daemon's main task. Because the runtime
/// is single-threaded (`current_thread`), nothing here needs to be `Send`
/// or wrapped in a mutex.
struct DaemonState {
    child_pid: libc::pid_t,
    pty_master_fd: RawFd, // owned by AsyncFd in daemon_main; held here for ioctl/write
    parser: vt100::Parser,
    session_name: String,
    socket_dir: std::path::PathBuf,
    shell_cmd: String,
    cwd: String,
    created_at: u64,
    task_ended_at: u64,
    task_exit_code: u8,
    child_exited: bool,
    has_had_client: bool,
    clients: HashMap<u64, mpsc::Sender<DaemonFrame>>,
    next_client_id: u64,
    last_client_disconnected_at: Option<u64>,
    empty_timeout: Option<u64>,
    old_session_names: Vec<String>,
    log_system: &'static crate::logger::LogSystem,
}

impl DaemonState {
    fn build_info(&self) -> ipc::Info {
        ipc::Info {
            clients_len: self.clients.len(),
            pid: self.child_pid,
            created_at: self.created_at,
            task_ended_at: self.task_ended_at,
            task_exit_code: self.task_exit_code,
            cmd: self.shell_cmd.as_bytes().to_vec(),
            cwd: self.cwd.as_bytes().to_vec(),
        }
    }

    /// Send a frame to every connected client. Drops clients whose channels
    /// are full (slow reader, exceeded backpressure budget) or closed.
    fn broadcast(&mut self, frame: DaemonFrame) {
        self.clients
            .retain(|_id, tx| tx.try_send(frame.clone()).is_ok());
    }

    /// Send a frame to a specific client. Drops the client on failure.
    fn send_to(&mut self, id: u64, frame: DaemonFrame) {
        let drop_it = match self.clients.get(&id) {
            Some(tx) => tx.try_send(frame).is_err(),
            None => false,
        };
        if drop_it {
            self.clients.remove(&id);
        }
    }

    /// Reap the child after SIGCHLD. Sets `child_exited` and records exit
    /// code; no-op if no child reaped yet (handler can fire on stops too).
    fn reap_child(&mut self) {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        if r > 0 {
            self.child_exited = true;
            self.task_exit_code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status) as u8
            } else {
                1
            };
            self.task_ended_at = now_epoch();
            log::info!(
                "child exited, pid={} exit_code={}",
                self.child_pid,
                self.task_exit_code
            );
        }
    }

    /// Feed PTY bytes into the parser, broadcast to clients, scan for the
    /// task-exit marker. Returns `true` if there are no clients attached
    /// (so the caller should answer any pending DA queries directly).
    fn on_pty_bytes(&mut self, data: &[u8]) -> bool {
        self.parser.process(data);
        if let Some(code) = util::find_task_exit_marker(data) {
            self.task_exit_code = code;
            self.task_ended_at = now_epoch();
            log::info!("task exit marker found, code={}", code);
        }
        self.broadcast(DaemonFrame {
            tag: Tag::Output,
            payload: Bytes::copy_from_slice(data),
        });
        self.clients.is_empty()
    }

    /// Register a newly-accepted client: allocate an id, set up the per-task
    /// mpsc channel, build the optional Init replay (only for second-and-on
    /// attaches — first attach gets live output from the freshly-spawned
    /// shell), then spawn the per-client task.
    fn accept_client(
        &mut self,
        stream: UnixStream,
        daemon_tx: mpsc::UnboundedSender<(u64, ClientMsg)>,
    ) {
        let id = self.next_client_id;
        self.next_client_id += 1;
        let (tx, rx) = mpsc::channel(CLIENT_TX_BUF);

        let initial = if self.has_had_client {
            util::serialize_terminal_state(&self.parser).map(|s| DaemonFrame {
                tag: Tag::Init,
                payload: Bytes::from(s),
            })
        } else {
            None
        };
        self.has_had_client = true;
        self.clients.insert(id, tx);
        log::info!("client connected, id={}", id);

        tokio::task::spawn_local(async move {
            client_task(stream, id, initial, rx, daemon_tx).await;
        });
    }

    /// Rename the live session: move its socket, SSH-auth-sock symlink, and
    /// log files to the new name, then re-init the logger. The old name is
    /// remembered so the symlink that points at the new name can be cleaned
    /// up on exit (existing clients reach the daemon via the old symlink
    /// pointing at the new one). No-op if `new_name` matches the current
    /// name or if the target socket path is already occupied.
    fn rename_session(&mut self, new_name: &str) {
        if new_name == self.session_name {
            return;
        }
        let old_socket_path = self.socket_dir.join(&self.session_name);
        let new_socket_path = self.socket_dir.join(new_name);

        if new_socket_path.exists() {
            log::error!(
                "rename failed: target socket path already exists: {}",
                new_socket_path.display()
            );
            return;
        }
        if let Err(e) = std::fs::rename(&old_socket_path, &new_socket_path) {
            log::error!("rename failed: failed to rename socket: {}", e);
            return;
        }
        log::info!(
            "session renamed: '{}' -> '{}'",
            self.session_name,
            new_name
        );

        // SSH-auth-sock: repoint the new-name symlink at the same target the
        // old one had, then make the old name a symlink to the new one so
        // already-attached clients keep resolving correctly.
        let old_symlink = self
            .socket_dir
            .join(format!("{}.ssh-auth-sock", self.session_name));
        let new_symlink = self.socket_dir.join(format!("{}.ssh-auth-sock", new_name));
        if let Ok(target) = std::fs::read_link(&old_symlink) {
            if new_symlink.exists() || new_symlink.is_symlink() {
                let _ = std::fs::remove_file(&new_symlink);
            }
            let _ = std::os::unix::fs::symlink(&target, &new_symlink);
            let _ = std::fs::remove_file(&old_symlink);
            let _ = std::os::unix::fs::symlink(&new_symlink, &old_symlink);
        }

        // Logs: rename current + rotated, then re-init the logger to point at
        // the new path. Failures are non-fatal — logging just stops working
        // and the user sees an error in stderr / current log.
        let logs_dir = self.socket_dir.join("logs");
        let old_log = logs_dir.join(format!("{}.log", self.session_name));
        let new_log = logs_dir.join(format!("{}.log", new_name));
        let old_log_rotated = logs_dir.join(format!("{}.log.old", self.session_name));
        let new_log_rotated = logs_dir.join(format!("{}.log.old", new_name));
        if old_log.exists() {
            let _ = std::fs::rename(&old_log, &new_log);
        }
        if old_log_rotated.exists() {
            let _ = std::fs::rename(&old_log_rotated, &new_log_rotated);
        }
        if let Err(e) = self.log_system.init(&new_log) {
            log::error!("failed to re-init log at {}: {}", new_log.display(), e);
        }

        self.old_session_names.push(self.session_name.clone());
        self.session_name = new_name.to_string();
    }

    /// Dispatch a parsed protocol frame from client `id`.
    fn handle_client_frame(&mut self, id: u64, tag: Tag, payload: Vec<u8>) {
        match tag {
            Tag::Input => {
                let _ = ipc::write_all(self.pty_master_fd, &payload);
            }
            Tag::Resize => {
                if let Some(r) = ipc::Resize::decode(&payload) {
                    self.parser.screen_mut().set_size(r.rows, r.cols);
                    let ws = libc::winsize {
                        ws_row: r.rows,
                        ws_col: r.cols,
                        ws_xpixel: 0,
                        ws_ypixel: 0,
                    };
                    unsafe {
                        libc::ioctl(self.pty_master_fd, libc::TIOCSWINSZ, &ws);
                    }
                }
            }
            Tag::Detach => {
                log::info!("client requested detach, id={}", id);
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Detach,
                        payload: Bytes::new(),
                    },
                );
                self.clients.remove(&id);
            }
            Tag::DetachAll => {
                log::info!("client requested detach-all");
                self.broadcast(DaemonFrame {
                    tag: Tag::Detach,
                    payload: Bytes::new(),
                });
                self.clients.clear();
            }
            Tag::Kill => {
                log::info!("kill requested");
                unsafe {
                    libc::kill(self.child_pid, libc::SIGTERM);
                }
            }
            Tag::Info => {
                let payload = Bytes::from(self.build_info().encode());
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Info,
                        payload,
                    },
                );
            }
            Tag::History => {
                let format = if payload.is_empty() {
                    util::HistoryFormat::Plain
                } else {
                    match payload[0] {
                        1 => util::HistoryFormat::Vt,
                        2 => util::HistoryFormat::Html,
                        _ => util::HistoryFormat::Plain,
                    }
                };
                let data = util::serialize_terminal(&self.parser, format).unwrap_or_default();
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::History,
                        payload: Bytes::from(data),
                    },
                );
            }
            Tag::Print => {
                if !payload.is_empty() {
                    self.parser.process(&payload);
                    self.broadcast(DaemonFrame {
                        tag: Tag::Output,
                        payload: Bytes::copy_from_slice(&payload),
                    });
                }
            }
            Tag::Run => {
                if !payload.is_empty() {
                    let _ = ipc::write_all(self.pty_master_fd, &payload);
                }
            }
            Tag::SshAuthSock => {
                if !payload.is_empty() {
                    if let Ok(path) = std::str::from_utf8(&payload) {
                        socket::update_ssh_auth_sock_symlink(
                            &self.socket_dir,
                            &self.session_name,
                            path,
                        );
                    }
                }
            }
            Tag::Rename => {
                if let Ok(new_name) = std::str::from_utf8(&payload) {
                    if !new_name.is_empty() {
                        self.rename_session(new_name);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Async event loop
// ---------------------------------------------------------------------------

/// Per-client task: owns the UnixStream, reads frames into the daemon via
/// `daemon_tx`, writes outbound frames received on its own channel. Uses
/// `RiftCodec` via FramedRead/FramedWrite so wire encoding/decoding lives
/// entirely in `ipc::RiftCodec`.
async fn client_task(
    stream: UnixStream,
    id: u64,
    initial: Option<DaemonFrame>,
    mut rx: mpsc::Receiver<DaemonFrame>,
    daemon_tx: mpsc::UnboundedSender<(u64, ClientMsg)>,
) {
    use futures_util::{SinkExt, StreamExt};

    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, RiftCodec);
    let mut writer = FramedWrite::new(write_half, RiftCodec);

    let write_join = tokio::task::spawn_local(async move {
        if let Some(initial) = initial {
            if writer.send((initial.tag, initial.payload)).await.is_err() {
                return;
            }
        }
        while let Some(frame) = rx.recv().await {
            if writer.send((frame.tag, frame.payload)).await.is_err() {
                break;
            }
        }
    });

    while let Some(item) = reader.next().await {
        let (tag, payload) = match item {
            Ok(f) => f,
            Err(_) => break,
        };
        if daemon_tx
            .send((
                id,
                ClientMsg::Frame {
                    tag,
                    payload: payload.to_vec(),
                },
            ))
            .is_err()
        {
            break;
        }
    }

    let _ = daemon_tx.send((id, ClientMsg::Gone));
    write_join.abort();
}

async fn daemon_main(mut state: DaemonState, listener: UnixListener, pty_master: OwnedFd) {
    let pty_async = match AsyncFd::new(pty_master) {
        Ok(fd) => fd,
        Err(e) => {
            log::error!("failed to wrap pty master in AsyncFd: {}", e);
            return;
        }
    };

    let mut sigchld = match signal(SignalKind::child()) {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to register SIGCHLD: {}", e);
            return;
        }
    };
    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            log::error!("failed to register SIGTERM: {}", e);
            return;
        }
    };

    let (daemon_tx, mut daemon_rx) = mpsc::unbounded_channel::<(u64, ClientMsg)>();
    let mut pty_buf = vec![0u8; PTY_READ_BUF];

    loop {
        // Compute deadline for empty-session self-termination, if armed.
        let empty_deadline = state
            .last_client_disconnected_at
            .zip(state.empty_timeout)
            .map(|(disc, lim)| {
                let elapsed = now_epoch().saturating_sub(disc);
                if elapsed >= lim {
                    Instant::now()
                } else {
                    Instant::now() + Duration::from_secs(lim - elapsed)
                }
            });

        tokio::select! {
            biased;

            _ = sigterm.recv() => {
                log::info!("SIGTERM received");
                break;
            }

            _ = sigchld.recv() => {
                state.reap_child();
            }

            Some((id, msg)) = daemon_rx.recv() => {
                match msg {
                    ClientMsg::Frame { tag, payload } => {
                        state.handle_client_frame(id, tag, payload);
                    }
                    ClientMsg::Gone => {
                        if state.clients.remove(&id).is_some() {
                            log::info!("client disconnected, id={}", id);
                        }
                    }
                }
            }

            ready = pty_async.readable() => {
                match ready {
                    Ok(mut guard) => {
                        let res = guard.try_io(|inner| {
                            let bfd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
                            unistd::read(&bfd, &mut pty_buf)
                                .map_err(|e| io::Error::from_raw_os_error(e as i32))
                        });
                        match res {
                            Ok(Ok(0)) => {
                                log::info!("pty master EOF");
                                break;
                            }
                            Ok(Ok(n)) => {
                                let no_clients = state.on_pty_bytes(&pty_buf[..n]);
                                if no_clients {
                                    util::respond_to_device_attributes(
                                        state.pty_master_fd,
                                        &pty_buf[..n],
                                    );
                                }
                            }
                            Ok(Err(e)) => {
                                if e.raw_os_error() == Some(libc::EIO) {
                                    log::info!("pty master EIO (child exited)");
                                    break;
                                }
                                log::warn!("pty read error: {}", e);
                                break;
                            }
                            Err(_would_block) => {
                                // false readiness; loop and re-await
                            }
                        }
                    }
                    Err(e) => {
                        log::error!("pty readable error: {}", e);
                        break;
                    }
                }
            }

            accept = listener.accept() => {
                match accept {
                    Ok((stream, _addr)) => state.accept_client(stream, daemon_tx.clone()),
                    Err(e) => log::warn!("accept error: {}", e),
                }
            }

            // Empty-session self-termination. When no deadline is armed,
            // this branch never fires (pending future).
            _ = async {
                match empty_deadline {
                    Some(d) => time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                log::info!(
                    "empty session timeout of {}s reached, self-terminating",
                    state.empty_timeout.unwrap_or(0)
                );
                break;
            }
        }

        // Track empty-state transitions for the timeout deadline.
        if state.clients.is_empty() {
            if state.has_had_client && state.last_client_disconnected_at.is_none() {
                state.last_client_disconnected_at = Some(now_epoch());
            }
        } else {
            state.last_client_disconnected_at = None;
        }

        if state.child_exited {
            // Drain a final non-blocking read so any trailing output reaches
            // attached clients before we tear down.
            let bfd = unsafe { BorrowedFd::borrow_raw(state.pty_master_fd) };
            if let Ok(n) = unistd::read(&bfd, &mut pty_buf) {
                if n > 0 {
                    state.on_pty_bytes(&pty_buf[..n]);
                }
            }
            break;
        }
    }

    // Notify any still-attached clients to detach gracefully.
    let detach = DaemonFrame {
        tag: Tag::Detach,
        payload: Bytes::new(),
    };
    for (_id, tx) in state.clients.drain() {
        let _ = tx.try_send(detach.clone());
    }

    // Clean up current active socket and symlink
    let active_socket = state.socket_dir.join(&state.session_name);
    let _ = std::fs::remove_file(active_socket);
    let active_symlink = state.socket_dir.join(format!("{}.ssh-auth-sock", state.session_name));
    let _ = std::fs::remove_file(active_symlink);

    // Clean up any historical/old symlinks left behind by rename
    for old_name in &state.old_session_names {
        let old_symlink = state.socket_dir.join(format!("{}.ssh-auth-sock", old_name));
        let _ = std::fs::remove_file(old_symlink);
    }
}

// ---------------------------------------------------------------------------
// Process-level entry points
// ---------------------------------------------------------------------------

fn run_daemon(cfg: &Cfg, server_fd: RawFd, cmd: &[String]) {
    ignore_signal(Signal::SIGPIPE);

    if let Ok(ssh_auth_sock) = std::env::var("SSH_AUTH_SOCK") {
        socket::update_ssh_auth_sock_symlink(&cfg.socket_dir, &cfg.session_name, &ssh_auth_sock);
    }

    let empty_timeout = std::env::var("RIFT_EMPTY_TIMEOUT")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    let shell = util::detect_shell();
    let spawn_cmd = if cmd.is_empty() { &shell } else { &cmd[0] };
    let spawn_args: Vec<&str> = if cmd.is_empty() {
        vec![]
    } else {
        cmd[1..].iter().map(|s| s.as_str()).collect()
    };
    let (master_fd, child_pid) =
        match spawn_pty(spawn_cmd, &spawn_args, 24, 80, &cfg.session_name) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: failed to spawn pty: {}", e);
                let _ = std::fs::remove_file(&cfg.socket_path);
                return;
            }
        };

    let early_output = drain_da_queries(master_fd);

    let log_system = Box::leak(Box::new(crate::logger::LogSystem::new()));
    let log_path = cfg
        .socket_dir
        .join("logs")
        .join(format!("{}.log", cfg.session_name));
    if let Err(e) = log_system.init(&log_path) {
        eprintln!("warning: failed to init log: {}", e);
    }
    let _ = log::set_logger(log_system);
    log::set_max_level(log::LevelFilter::Info);

    log::info!("daemon starting, session={}", cfg.session_name);

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let display_cmd = if cmd.is_empty() {
        shell.clone()
    } else {
        cmd.join(" ")
    };
    log::info!("child spawned, pid={} cmd={}", child_pid, display_cmd);

    let mut parser = vt100::Parser::new(24, 80, 1000);
    if !early_output.is_empty() {
        parser.process(&early_output);
    }

    let state = DaemonState {
        child_pid,
        pty_master_fd: master_fd,
        parser,
        session_name: cfg.session_name.clone(),
        socket_dir: cfg.socket_dir.clone(),
        shell_cmd: display_cmd,
        cwd,
        created_at: now_epoch(),
        task_ended_at: 0,
        task_exit_code: 0,
        child_exited: false,
        has_had_client: false,
        clients: HashMap::new(),
        next_client_id: 0,
        last_client_disconnected_at: None,
        empty_timeout,
        old_session_names: Vec::new(),
        log_system,
    };

    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(server_fd) };
    if let Err(e) = std_listener.set_nonblocking(true) {
        log::error!("failed to set listener nonblock: {}", e);
        return;
    }

    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log::error!("failed to build runtime: {}", e);
            return;
        }
    };

    let local = tokio::task::LocalSet::new();
    let session_name = cfg.session_name.clone();
    local.block_on(&rt, async move {
        let listener = match UnixListener::from_std(std_listener) {
            Ok(l) => l,
            Err(e) => {
                log::error!("failed to convert listener: {}", e);
                return;
            }
        };
        // SAFETY: master_fd was just produced by spawn_pty and is not owned
        // elsewhere. OwnedFd will close it when AsyncFd is dropped.
        let pty_owned = unsafe { OwnedFd::from_raw_fd(master_fd) };
        daemon_main(state, listener, pty_owned).await;
    });

    log::info!("daemon exiting, session={}", session_name);
}

fn fork_daemon(cfg: &Cfg, cmd: &[String]) -> Result<(), String> {
    let server_owned = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;
    let server_fd = server_owned.into_raw_fd();

    let cmd_owned: Vec<String> = cmd.to_vec();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(server_fd);
        }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        unsafe {
            libc::setsid();
        }
        redirect_std_to_devnull();
        run_daemon(cfg, server_fd, &cmd_owned);
        unsafe {
            libc::_exit(0);
        }
    }

    unsafe {
        libc::close(server_fd);
    }
    Ok(())
}

pub fn spawn_daemon(cfg: &Cfg, cmd: &[String]) -> Result<OwnedFd, String> {
    fork_daemon(cfg, cmd)?;

    let path_str = cfg.socket_path.to_str().ok_or("invalid socket path")?;

    for i in 0..20 {
        match socket::session_connect(path_str) {
            Ok(fd) => return Ok(fd),
            Err(_) if i < 19 => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(format!("failed to connect to new session: {}", e)),
        }
    }
    unreachable!()
}

pub fn spawn_daemon_detached(cfg: &Cfg, cmd: &[String]) -> Result<(), String> {
    fork_daemon(cfg, cmd)?;
    println!("session '{}' created", cfg.session_name);
    Ok(())
}
