//! Daemon-process side: owns the PTY, accepts client connections, drives
//! the terminal-state model, and brokers per-client tasks via mpsc channels.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::Signal;
use nix::unistd;
use tokio::io::unix::AsyncFd;
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::{self, Duration, Instant};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::ipc::{self, RiftCodec, Tag};
use crate::label;
use crate::socket;
use crate::util;

use super::{Cfg, ignore_signal};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Per-client outgoing channel cap. Above this, the slow client is dropped
/// to prevent unbounded memory growth.
const CLIENT_TX_BUF: usize = 256;
const PTY_READ_BUF: usize = 4096;
/// Keep PTY input bounded when the foreground process temporarily stops
/// reading. New payloads are rejected as a unit rather than truncating an
/// already-accepted escape sequence or command.
const PTY_WRITE_BUF_MAX: usize = 256 * 1024;
const FOCUS_IN: &[u8] = b"\x1b[I";
const FOCUS_OUT: &[u8] = b"\x1b[O";

// ---------------------------------------------------------------------------
// Low-level helpers (private to the server side)
// ---------------------------------------------------------------------------

fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    unistd::read(bfd, buf)
}

fn close_inherited_fds(keep_fd: RawFd) {
    // stdio has already been redirected. Close unrelated descriptors inherited
    // from shells, test harnesses, and editor integrations so the daemon cannot
    // keep their pipes or sockets alive. RLIMIT provides a portable upper bound.
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    let max_fd = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } == 0 {
        limit.rlim_cur.min(65_536) as RawFd
    } else {
        1024
    };
    for fd in 3..max_fd {
        if fd != keep_fd {
            unsafe { libc::close(fd) };
        }
    }
}

fn redirect_std_to_devnull() {
    unsafe {
        let devnull = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
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
    login_shell: bool,
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

            let term_key = c"TERM";
            let term = libc::getenv(term_key.as_ptr());
            if term.is_null() || std::ffi::CStr::from_ptr(term).to_bytes() == b"dumb" {
                libc::setenv(term_key.as_ptr(), c"xterm-256color".as_ptr(), 1);
            }

            let sock_dir = socket::socket_dir();
            let symlink_path = sock_dir.join(format!("{}.ssh-auth-sock", session_name));
            if let Some(symlink_str) = symlink_path.to_str() {
                let key_ssh = std::ffi::CString::new("SSH_AUTH_SOCK").unwrap();
                let val_ssh = std::ffi::CString::new(symlink_str).unwrap();
                libc::setenv(key_ssh.as_ptr(), val_ssh.as_ptr(), 1);
            }

            libc::signal(libc::SIGPIPE, libc::SIG_DFL);

            if login_shell {
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
                libc::execvp(cmd_cstr.as_ptr(), argv_ptrs.as_ptr());
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
    Request(ipc::ClientRequest),
    /// Read loop ended (socket EOF, error, or write task crashed).
    Gone,
}

/// Frame queued for delivery to a client task's socket.
#[derive(Clone)]
struct DaemonFrame {
    tag: Tag,
    payload: Bytes,
}

type ClientId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientKind {
    Control,
    Terminal,
    OutputSubscriber,
}

struct ClientConnection {
    sender: mpsc::Sender<DaemonFrame>,
    kind: ClientKind,
    environment: Option<String>,
}

#[derive(Default)]
struct ClientRegistry {
    clients: HashMap<ClientId, ClientConnection>,
    /// Initialization order gives deterministic leader promotion when the
    /// current terminal disconnects.
    terminal_order: VecDeque<ClientId>,
    leader: Option<ClientId>,
}

struct RemovedClient {
    was_terminal: bool,
    new_leader: Option<ClientId>,
}

impl ClientRegistry {
    fn insert(&mut self, id: ClientId, sender: mpsc::Sender<DaemonFrame>) {
        self.clients.insert(
            id,
            ClientConnection {
                sender,
                kind: ClientKind::Control,
                environment: None,
            },
        );
    }

    fn sender(&self, id: ClientId) -> Option<&mpsc::Sender<DaemonFrame>> {
        self.clients.get(&id).map(|client| &client.sender)
    }

    fn initialize_terminal(&mut self, id: ClientId) -> bool {
        let Some(client) = self.clients.get_mut(&id) else {
            return false;
        };
        match client.kind {
            ClientKind::Terminal => false,
            ClientKind::Control => {
                client.kind = ClientKind::Terminal;
                self.terminal_order.push_back(id);
                true
            }
            ClientKind::OutputSubscriber => false,
        }
    }

    fn subscribe_tail(&mut self, id: ClientId) {
        if let Some(client) = self.clients.get_mut(&id)
            && client.kind != ClientKind::Terminal
        {
            client.kind = ClientKind::OutputSubscriber;
        }
    }

    fn is_terminal(&self, id: ClientId) -> bool {
        self.clients
            .get(&id)
            .is_some_and(|client| client.kind == ClientKind::Terminal)
    }

    fn terminal_count(&self) -> usize {
        self.terminal_order.len()
    }

    fn has_terminals(&self) -> bool {
        !self.terminal_order.is_empty()
    }

    fn leader(&self) -> Option<ClientId> {
        self.leader
    }

    fn set_leader(&mut self, id: ClientId) -> bool {
        if !self.is_terminal(id) || self.leader == Some(id) {
            return false;
        }
        self.leader = Some(id);
        true
    }

    fn output_recipients(&self) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter_map(|(id, client)| {
                matches!(
                    client.kind,
                    ClientKind::Terminal | ClientKind::OutputSubscriber
                )
                .then_some(*id)
            })
            .collect()
    }

    fn all_client_ids(&self) -> Vec<ClientId> {
        self.clients.keys().copied().collect()
    }

    fn set_environment(&mut self, id: ClientId, environment: Option<String>) {
        if let Some(client) = self.clients.get_mut(&id) {
            client.environment = environment;
        }
    }

    fn selected_environment(&self) -> Option<&str> {
        self.leader
            .and_then(|leader| self.clients.get(&leader))
            .and_then(|client| client.environment.as_deref())
            .or_else(|| {
                let mut environments = self
                    .clients
                    .values()
                    .filter_map(|client| client.environment.as_deref());
                let only = environments.next()?;
                environments.next().is_none().then_some(only)
            })
    }

    fn remove(&mut self, id: ClientId) -> Option<RemovedClient> {
        let client = self.clients.remove(&id)?;
        let was_terminal = client.kind == ClientKind::Terminal;
        if was_terminal {
            self.terminal_order.retain(|client_id| *client_id != id);
        }
        let new_leader = if self.leader == Some(id) {
            self.leader = self.terminal_order.front().copied();
            self.leader
        } else {
            None
        };
        Some(RemovedClient {
            was_terminal,
            new_leader,
        })
    }

    fn clear(&mut self) -> bool {
        let had_terminals = self.has_terminals();
        self.clients.clear();
        self.terminal_order.clear();
        self.leader = None;
        had_terminals
    }

    fn drain_senders(&mut self) -> impl Iterator<Item = mpsc::Sender<DaemonFrame>> + '_ {
        self.terminal_order.clear();
        self.leader = None;
        self.clients.drain().map(|(_, client)| client.sender)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputAction {
    Drop,
    Forward,
    TakeLeadership,
}

fn input_action(
    leader_client_id: Option<u64>,
    client_id: u64,
    interactive: bool,
    payload: &[u8],
) -> InputAction {
    if leader_client_id == Some(client_id) || !interactive {
        InputAction::Forward
    } else if util::is_user_input(payload) {
        InputAction::TakeLeadership
    } else {
        InputAction::Drop
    }
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
    parser: crate::term_state::TermState,
    session_name: String,
    socket_dir: std::path::PathBuf,
    shell_cmd: String,
    cwd: String,
    created_at: u64,
    task_ended_at: u64,
    task_exit_code: u8,
    child_exited: bool,
    shutdown_requested: bool,
    has_pty_output: bool,
    has_had_terminal_client: bool,
    clients: ClientRegistry,
    labels: BTreeMap<String, String>,
    pending_runs: HashMap<u64, u64>,
    task_scan_carry: Vec<u8>,
    pty_write_buf: VecDeque<u8>,
    next_client_id: u64,
    last_client_disconnected_at: Option<u64>,
    empty_timeout: Option<u64>,
    old_session_names: Vec<String>,
    log_system: &'static crate::logger::LogSystem,
}

impl DaemonState {
    fn build_info(&self) -> ipc::Info {
        // Preserve the complete OSC 7 URI (including remote host) for list
        // output; startup directories remain plain paths for older sessions.
        let cwd = self.parser.cwd_uri().unwrap_or_else(|| self.cwd.clone());
        ipc::Info {
            clients_len: self.clients.terminal_count(),
            pid: self.child_pid,
            created_at: self.created_at,
            task_ended_at: self.task_ended_at,
            task_exit_code: self.task_exit_code,
            cmd: self.shell_cmd.as_bytes().to_vec(),
            cwd: cwd.into_bytes(),
        }
    }

    /// Send PTY output only to clients which explicitly subscribed by attaching
    /// a terminal or issuing a Tail request. Slow/closed clients are removed.
    fn broadcast_output(&mut self, frame: DaemonFrame) {
        let recipients = self.clients.output_recipients();
        for id in recipients {
            self.send_to(id, frame.clone());
        }
    }

    /// Send a frame to every connected client, used for daemon-wide control
    /// events such as detach-all.
    fn broadcast_control(&mut self, frame: DaemonFrame) {
        let recipients = self.clients.all_client_ids();
        for id in recipients {
            self.send_to(id, frame.clone());
        }
    }

    /// Send a frame to a specific client. Drops the client on failure.
    fn send_to(&mut self, id: ClientId, frame: DaemonFrame) {
        let drop_it = self
            .clients
            .sender(id)
            .is_some_and(|sender| sender.try_send(frame).is_err());
        if drop_it {
            self.remove_client(id);
        }
    }

    fn remove_client(&mut self, id: ClientId) -> bool {
        let Some(removed) = self.clients.remove(id) else {
            return false;
        };
        if let Some(next) = removed.new_leader {
            log::info!("interactive leader disconnected, id={}", id);
            self.request_leader_size(next);
        }
        if removed.was_terminal && !self.clients.has_terminals() && self.parser.focus_reporting() {
            self.queue_pty_input(FOCUS_OUT);
        }
        true
    }

    fn queue_pty_input(&mut self, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        if self.pty_write_buf.len() + data.len() > PTY_WRITE_BUF_MAX {
            log::warn!(
                "pty input dropped {} bytes ({} byte buffer full)",
                data.len(),
                PTY_WRITE_BUF_MAX
            );
            return false;
        }
        self.pty_write_buf.extend(data);
        true
    }

    fn apply_resize(&mut self, resize: ipc::Resize) {
        self.parser.set_size(resize.rows, resize.cols);
        let ws = libc::winsize {
            ws_row: resize.rows,
            ws_col: resize.cols,
            // Forward the client's reported pixel dimensions so graphical
            // programs in the session (Kitty/sixel image viewers, etc.) can
            // compute cell pixel sizes. 0 means the client's terminal didn't
            // report them.
            ws_xpixel: resize.xpixel,
            ws_ypixel: resize.ypixel,
        };
        unsafe {
            libc::ioctl(self.pty_master_fd, libc::TIOCSWINSZ, &ws);
        }
    }

    fn signal_foreground(&self, signal: libc::c_int) {
        let mut pgrp: libc::pid_t = 0;
        let got_pgrp = unsafe { libc::ioctl(self.pty_master_fd, libc::TIOCGPGRP, &mut pgrp) } == 0;
        if got_pgrp && pgrp > 0 {
            unsafe {
                libc::kill(-pgrp, signal);
            }
        }
    }

    fn set_leader(&mut self, id: ClientId) {
        if self.clients.set_leader(id) {
            log::info!("interactive leader changed, id={}", id);
            self.request_leader_size(id);
        }
    }

    fn request_leader_size(&mut self, id: ClientId) {
        // Ask for a fresh size. Cached dimensions can be stale when a terminal
        // was resized while it was not the interactive leader.
        self.send_to(
            id,
            DaemonFrame {
                tag: Tag::Resize,
                payload: Bytes::new(),
            },
        );
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
        self.has_pty_output = true;
        self.parser.process(data);
        for (request_id, code) in util::scan_task_completions(&mut self.task_scan_carry, data) {
            self.task_exit_code = code;
            self.task_ended_at = now_epoch();
            log::info!(
                "task completed, request_id={} exit_code={}",
                request_id,
                code
            );
            if let Some(client_id) = self.pending_runs.remove(&request_id) {
                self.send_to(
                    client_id,
                    DaemonFrame {
                        tag: Tag::TaskComplete,
                        payload: ipc::encode_task_complete(request_id, code),
                    },
                );
            }
        }
        let rewritten = util::rewrite_prompt_redraw(data);
        let output = rewritten.as_deref().unwrap_or(data);
        self.broadcast_output(DaemonFrame {
            tag: Tag::Output,
            payload: Bytes::copy_from_slice(output),
        });
        !self.clients.has_terminals()
    }

    /// Register a newly-accepted socket. It does not count as an attached
    /// terminal until it sends a valid Init frame with its terminal dimensions.
    fn accept_client(
        &mut self,
        stream: UnixStream,
        daemon_tx: mpsc::UnboundedSender<(ClientId, ClientMsg)>,
    ) {
        let id = self.next_client_id;
        self.next_client_id += 1;
        let (tx, rx) = mpsc::channel(CLIENT_TX_BUF);
        self.clients.insert(id, tx);
        log::info!("client connected, id={}", id);

        tokio::task::spawn_local(async move {
            client_task(stream, id, rx, daemon_tx).await;
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
        log::info!("session renamed: '{}' -> '{}'", self.session_name, new_name);

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

    /// Dispatch a validated protocol request from client `id`.
    fn handle_client_request(&mut self, id: ClientId, request: ipc::ClientRequest) {
        use ipc::ClientRequest;

        match request {
            ClientRequest::Input(payload) => {
                let interactive = self.clients.is_terminal(id);
                match input_action(self.clients.leader(), id, interactive, &payload) {
                    InputAction::Drop => {}
                    InputAction::Forward => {
                        self.queue_pty_input(&payload);
                    }
                    InputAction::TakeLeadership => {
                        self.set_leader(id);
                        self.queue_pty_input(&payload);
                    }
                }
            }
            ClientRequest::AttachTerminal(r) => {
                let was_headless = !self.clients.has_terminals();
                let first_init = self.clients.initialize_terminal(id);
                if self.clients.leader().is_none() {
                    self.set_leader(id);
                }
                let is_leader = self.clients.leader() == Some(id);
                if is_leader {
                    // Lay out the snapshot for the attaching terminal before
                    // serializing, then resize the real PTY and force WINCH.
                    self.parser.set_size(r.rows, r.cols);
                }
                if first_init
                    && self.has_pty_output
                    && self.has_had_terminal_client
                    && let Some(state) = self.parser.serialize_state()
                {
                    let state = util::rewrite_prompt_redraw(&state).unwrap_or(state);
                    self.send_to(
                        id,
                        DaemonFrame {
                            tag: Tag::Init,
                            payload: Bytes::from(state),
                        },
                    );
                }
                if is_leader {
                    self.apply_resize(r);
                    if first_init && self.has_had_terminal_client {
                        self.signal_foreground(libc::SIGWINCH);
                    }
                }
                if first_init {
                    self.has_had_terminal_client = true;
                }
                if was_headless && self.parser.focus_reporting() {
                    self.queue_pty_input(FOCUS_IN);
                }
            }
            ClientRequest::Resize(r) => {
                if self.clients.leader() == Some(id) || self.clients.leader().is_none() {
                    self.apply_resize(r);
                }
            }
            ClientRequest::Detach => {
                log::info!("client requested detach, id={}", id);
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Detach,
                        payload: Bytes::new(),
                    },
                );
                self.remove_client(id);
            }
            ClientRequest::DetachAll => {
                log::info!("client requested detach-all");
                self.broadcast_control(DaemonFrame {
                    tag: Tag::Detach,
                    payload: Bytes::new(),
                });
                let had_terminals = self.clients.clear();
                if had_terminals && self.parser.focus_reporting() {
                    self.queue_pty_input(FOCUS_OUT);
                }
            }
            ClientRequest::Kill => {
                // The event loop sees this flag, unlinks the listener before
                // terminating the process group, and then exits.
                log::info!("kill requested");
                self.shutdown_requested = true;
            }
            ClientRequest::Info => {
                let payload = Bytes::from(self.build_info().encode());
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Info,
                        payload,
                    },
                );
            }
            ClientRequest::LabelGet => {
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::LabelData,
                        payload: Bytes::from(label::encode(&self.labels)),
                    },
                );
            }
            ClientRequest::LabelSet(pairs) => {
                for pair in pairs.split_whitespace() {
                    let Ok((key, value)) = label::parse_pair(pair) else {
                        continue;
                    };
                    if value.is_empty() {
                        self.labels.remove(key);
                    } else {
                        self.labels.insert(key.to_string(), value.to_string());
                    }
                }
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Ack,
                        payload: Bytes::new(),
                    },
                );
            }
            ClientRequest::LabelClear => {
                self.labels.clear();
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::Ack,
                        payload: Bytes::new(),
                    },
                );
            }
            ClientRequest::History(format) => {
                let data = util::serialize_terminal(&self.parser, format).unwrap_or_default();
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::History,
                        payload: Bytes::from(data),
                    },
                );
            }
            ClientRequest::Print(payload) => {
                self.parser.process(&payload);
                self.broadcast_output(DaemonFrame {
                    tag: Tag::Output,
                    payload,
                });
            }
            ClientRequest::SubscribeOutput => self.clients.subscribe_tail(id),
            ClientRequest::Run(payload) => {
                self.clients.subscribe_tail(id);
                let request_id = util::task_request_id(&payload);
                if let Some(request_id) = request_id {
                    self.pending_runs.insert(request_id, id);
                }
                if !self.queue_pty_input(&payload)
                    && let Some(request_id) = request_id
                {
                    self.pending_runs.remove(&request_id);
                    self.send_to(
                        id,
                        DaemonFrame {
                            tag: Tag::TaskComplete,
                            payload: ipc::encode_task_complete(request_id, 255),
                        },
                    );
                }
            }
            ClientRequest::SshAuthSock(path) => {
                socket::update_ssh_auth_sock_symlink(&self.socket_dir, &self.session_name, &path);
            }
            ClientRequest::EnvSet(environment) => {
                // A terminal client reported its tracked environment snapshot.
                self.clients.set_environment(id, environment);
            }
            ClientRequest::EnvGet => {
                // Report the leader terminal's environment, falling back to a
                // lone reported snapshot before leadership is established.
                let payload = self
                    .clients
                    .selected_environment()
                    .unwrap_or_default()
                    .to_string();
                self.send_to(
                    id,
                    DaemonFrame {
                        tag: Tag::EnvData,
                        payload: Bytes::from(payload.into_bytes()),
                    },
                );
            }
            ClientRequest::Rename(new_name) => self.rename_session(&new_name),
            ClientRequest::Switch(target) => {
                // A client (typically a transient `rift attach <target>` run
                // from *inside* the session) wants the interactive user handed
                // off to another session. Relay the target name plus this
                // session's live cwd (`name\ncwd`) to the current leader client
                // so it detaches from us and attaches to the target, spawning it
                // in the right directory if it doesn't exist yet.
                if let Some(leader) = self.clients.leader() {
                    log::info!("client {} requested switch to '{}'", id, target);
                    let cwd = self.parser.cwd().unwrap_or_else(|| self.cwd.clone());
                    self.send_to(
                        leader,
                        DaemonFrame {
                            tag: Tag::Switch,
                            payload: ipc::encode_switch(&target, Some(&cwd)),
                        },
                    );
                    self.remove_client(leader);
                }
            }
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
    mut rx: mpsc::Receiver<DaemonFrame>,
    daemon_tx: mpsc::UnboundedSender<(u64, ClientMsg)>,
) {
    use futures_util::{SinkExt, StreamExt};

    let (read_half, write_half) = stream.into_split();
    let mut reader = FramedRead::new(read_half, RiftCodec);
    let mut writer = FramedWrite::new(write_half, RiftCodec);

    let write_join = tokio::task::spawn_local(async move {
        while let Some(frame) = rx.recv().await {
            if writer.send((frame.tag, frame.payload)).await.is_err() {
                break;
            }
        }
    });

    while let Some(item) = reader.next().await {
        let (tag, payload) = match item {
            Ok(f) => f,
            Err(error) => {
                log::warn!("client {} sent an invalid frame: {}", id, error);
                break;
            }
        };
        let request = match ipc::ClientRequest::decode(tag, payload) {
            Ok(request) => request,
            Err(error) => {
                log::warn!("client {} sent an invalid request: {}", id, error);
                break;
            }
        };
        if daemon_tx.send((id, ClientMsg::Request(request))).is_err() {
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
                state.shutdown_requested = true;
            }

            _ = sigchld.recv() => {
                state.reap_child();
            }

            Some((id, msg)) = daemon_rx.recv() => {
                match msg {
                    ClientMsg::Request(request) => {
                        state.handle_client_request(id, request);
                    }
                    ClientMsg::Gone => {
                        if state.remove_client(id) {
                            log::info!("client disconnected, id={}", id);
                        }
                    }
                }
            }

            ready = pty_async.writable(), if !state.pty_write_buf.is_empty() => {
                match ready {
                    Ok(mut guard) => {
                        let (front, _) = state.pty_write_buf.as_slices();
                        let result = guard.try_io(|inner| {
                            let bfd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
                            unistd::write(bfd, front)
                                .map_err(|e| io::Error::from_raw_os_error(e as i32))
                        });
                        match result {
                            Ok(Ok(n)) => { state.pty_write_buf.drain(..n); }
                            Ok(Err(e)) => {
                                log::warn!("pty write error: {}", e);
                                break;
                            }
                            Err(_) => {}
                        }
                    }
                    Err(e) => {
                        log::error!("pty writable error: {}", e);
                        break;
                    }
                }
            }

            ready = pty_async.readable() => {
                match ready {
                    Ok(mut guard) => {
                        let res = guard.try_io(|inner| {
                            let bfd = unsafe { BorrowedFd::borrow_raw(inner.get_ref().as_raw_fd()) };
                            unistd::read(bfd, &mut pty_buf)
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
        if !state.clients.has_terminals() {
            if state.has_had_terminal_client && state.last_client_disconnected_at.is_none() {
                state.last_client_disconnected_at = Some(now_epoch());
            }
        } else {
            state.last_client_disconnected_at = None;
        }

        if state.shutdown_requested {
            // Stop accepting under this name before process termination so an
            // immediate recreation cannot connect to a dying daemon's backlog.
            let active_socket = state.socket_dir.join(&state.session_name);
            let _ = std::fs::remove_file(active_socket);
            let active_symlink = state
                .socket_dir
                .join(format!("{}.ssh-auth-sock", state.session_name));
            let _ = std::fs::remove_file(active_symlink);
            break;
        }

        if state.child_exited {
            // Drain a final non-blocking read so any trailing output reaches
            // attached clients before we tear down.
            let bfd = unsafe { BorrowedFd::borrow_raw(state.pty_master_fd) };
            if let Ok(n) = unistd::read(bfd, &mut pty_buf)
                && n > 0
            {
                state.on_pty_bytes(&pty_buf[..n]);
            }
            break;
        }
    }

    // A requested shutdown owns the entire PTY foreground process group, not
    // merely the shell PID. Shells commonly ignore SIGTERM; SIGHUP followed by
    // a bounded grace period and SIGKILL matches terminal hangup semantics.
    if state.shutdown_requested {
        state.signal_foreground(libc::SIGHUP);
        time::sleep(Duration::from_millis(500)).await;
        state.signal_foreground(libc::SIGKILL);
    }

    // Notify any still-attached clients to detach gracefully.
    let detach = DaemonFrame {
        tag: Tag::Detach,
        payload: Bytes::new(),
    };
    for sender in state.clients.drain_senders() {
        let _ = sender.try_send(detach.clone());
    }

    // Normal child exit still needs cleanup. Requested shutdown unlinked these
    // before its grace period; do not remove the same pathname again because a
    // replacement daemon may already own it.
    if !state.shutdown_requested {
        let active_socket = state.socket_dir.join(&state.session_name);
        let _ = std::fs::remove_file(active_socket);
        let active_symlink = state
            .socket_dir
            .join(format!("{}.ssh-auth-sock", state.session_name));
        let _ = std::fs::remove_file(active_symlink);
    }

    // Clean up any historical/old symlinks left behind by rename
    for old_name in &state.old_session_names {
        let old_symlink = state.socket_dir.join(format!("{}.ssh-auth-sock", old_name));
        let _ = std::fs::remove_file(old_symlink);
    }

    util::run_hook("RIFT_ON_EXIT", &state.session_name);
}

// ---------------------------------------------------------------------------
// Process-level entry points
// ---------------------------------------------------------------------------

fn run_daemon(cfg: &Cfg, server_fd: RawFd, cmd: &[String], initial_labels: &[String]) {
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
    let (master_fd, child_pid) = match spawn_pty(
        spawn_cmd,
        &spawn_args,
        cmd.is_empty(),
        24,
        80,
        &cfg.session_name,
    ) {
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

    let mut parser = crate::term_state::TermState::new(24, 80);
    if !early_output.is_empty() {
        parser.process(&early_output);
    }

    // Apply any create-time labels (from `attach --labels`) atomically before
    // the daemon serves clients, so a label filter never sees the session
    // unlabeled. Malformed pairs are ignored here — the CLI validates first.
    let mut labels: BTreeMap<String, String> = BTreeMap::new();
    for group in initial_labels {
        for pair in group.split_whitespace() {
            if let Ok((key, value)) = crate::label::parse_pair(pair) {
                if value.is_empty() {
                    labels.remove(key);
                } else {
                    labels.insert(key.to_string(), value.to_string());
                }
            }
        }
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
        shutdown_requested: false,
        has_pty_output: !early_output.is_empty(),
        has_had_terminal_client: false,
        clients: ClientRegistry::default(),
        labels,
        pending_runs: HashMap::new(),
        task_scan_carry: Vec::new(),
        pty_write_buf: VecDeque::new(),
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

fn fork_daemon(cfg: &Cfg, cmd: &[String], labels: &[String]) -> Result<(), String> {
    let server_owned = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;
    let server_fd = server_owned.into_raw_fd();

    let cmd_owned: Vec<String> = cmd.to_vec();
    let labels_owned: Vec<String> = labels.to_vec();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe {
            libc::close(server_fd);
        }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        if unsafe { libc::setsid() } < 0 {
            unsafe { libc::_exit(1) };
        }
        // A second fork ensures the daemon is not a session leader and can
        // never accidentally reacquire a controlling terminal.
        let daemon_pid = unsafe { libc::fork() };
        if daemon_pid < 0 {
            unsafe { libc::_exit(1) };
        }
        if daemon_pid > 0 {
            unsafe { libc::_exit(0) };
        }
        redirect_std_to_devnull();
        close_inherited_fds(server_fd);
        run_daemon(cfg, server_fd, &cmd_owned, &labels_owned);
        unsafe {
            libc::_exit(0);
        }
    }

    unsafe {
        libc::close(server_fd);
        // Reap the short-lived session leader from the first fork.
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    Ok(())
}

pub fn spawn_daemon(cfg: &Cfg, cmd: &[String], labels: &[String]) -> Result<OwnedFd, String> {
    fork_daemon(cfg, cmd, labels)?;

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

pub fn spawn_daemon_detached(cfg: &Cfg, cmd: &[String], labels: &[String]) -> Result<(), String> {
    fork_daemon(cfg, cmd, labels)?;
    println!("session '{}' created", cfg.session_name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with_clients(ids: &[ClientId]) -> ClientRegistry {
        let mut registry = ClientRegistry::default();
        for id in ids {
            let (sender, _receiver) = mpsc::channel(1);
            registry.insert(*id, sender);
        }
        registry
    }

    #[test]
    fn registry_routes_output_only_to_terminals_and_tail_subscribers() {
        let mut registry = registry_with_clients(&[1, 2, 3]);
        assert!(registry.initialize_terminal(1));
        registry.subscribe_tail(2);

        let mut recipients = registry.output_recipients();
        recipients.sort_unstable();
        assert_eq!(recipients, vec![1, 2]);
        assert_eq!(registry.terminal_count(), 1);
    }

    #[test]
    fn registry_promotes_terminals_in_initialization_order() {
        let mut registry = registry_with_clients(&[1, 2, 3]);
        assert!(registry.initialize_terminal(2));
        assert!(registry.initialize_terminal(1));
        assert!(registry.set_leader(2));

        let removed = registry.remove(2).expect("leader exists");
        assert!(removed.was_terminal);
        assert_eq!(removed.new_leader, Some(1));
        assert_eq!(registry.leader(), Some(1));
    }

    #[test]
    fn registry_prefers_leader_environment() {
        let mut registry = registry_with_clients(&[1, 2]);
        assert!(registry.initialize_terminal(1));
        assert!(registry.initialize_terminal(2));
        registry.set_environment(1, Some("DISPLAY=:1".to_string()));
        registry.set_environment(2, Some("DISPLAY=:2".to_string()));
        assert!(registry.set_leader(2));

        assert_eq!(registry.selected_environment(), Some("DISPLAY=:2"));
    }

    #[test]
    fn noninteractive_input_does_not_take_leadership() {
        assert_eq!(
            input_action(Some(1), 2, false, b"automated input"),
            InputAction::Forward
        );
    }

    #[test]
    fn interactive_keyboard_input_takes_leadership() {
        assert_eq!(
            input_action(Some(1), 2, true, b"\x1b[102;1:1u"),
            InputAction::TakeLeadership
        );
    }

    #[test]
    fn interactive_terminal_response_does_not_take_leadership() {
        assert_eq!(
            input_action(Some(1), 2, true, b"\x1b[?1;2c"),
            InputAction::Drop
        );
    }
}
