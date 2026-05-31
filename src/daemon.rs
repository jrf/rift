use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use nix::sys::termios::{self, SetArg, Termios};
use nix::unistd;

use crate::ipc::{self, SocketBuffer, Tag};
use crate::socket;
use crate::util;

// ---------------------------------------------------------------------------
// Cfg — session configuration
// ---------------------------------------------------------------------------

pub struct Cfg {
    pub session_name: String,
    pub socket_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl Cfg {
    pub fn resolve(name: &str) -> Result<Self, String> {
        let prefix = socket::session_prefix();
        let session_name = socket::get_session_name(&prefix, name).map_err(|e| format!("{}", e))?;
        let socket_dir = socket::socket_dir();
        let socket_path = socket::get_socket_path(&socket_dir, &session_name).map_err(|_| {
            socket::print_session_name_too_long(&session_name, &socket_dir);
            "socket path too long".to_string()
        })?;
        Ok(Cfg {
            session_name,
            socket_dir,
            socket_path,
        })
    }
}

// ---------------------------------------------------------------------------
// Raw helpers
// ---------------------------------------------------------------------------

pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
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

pub fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<()> {
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };

    let fl = fcntl(&bfd, FcntlArg::F_GETFL).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let fl = OFlag::from_bits_truncate(fl) | OFlag::O_NONBLOCK;
    fcntl(&bfd, FcntlArg::F_SETFL(fl)).map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    let fd_flags =
        fcntl(&bfd, FcntlArg::F_GETFD).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let fd_flags = FdFlag::from_bits_truncate(fd_flags) | FdFlag::FD_CLOEXEC;
    fcntl(&bfd, FcntlArg::F_SETFD(fd_flags)).map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Self-pipe signal trick
// ---------------------------------------------------------------------------

static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn signal_handler(_sig: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            let byte: u8 = 1;
            libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        }
    }
}

pub fn create_signal_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    set_nonblock_and_cloexec(fds[0])?;
    set_nonblock_and_cloexec(fds[1])?;
    SIGNAL_FD.store(fds[1], Ordering::Relaxed);
    Ok((fds[0], fds[1]))
}

pub fn install_signal_handler(sig: Signal) {
    let sa = SigAction::new(
        SigHandler::Handler(signal_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe {
        let _ = sigaction(sig, &sa);
    }
}

pub fn ignore_signal(sig: Signal) {
    let sa = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe {
        let _ = sigaction(sig, &sa);
    }
}

// ---------------------------------------------------------------------------
// Terminal raw mode
// ---------------------------------------------------------------------------

pub fn enter_raw_mode(fd: RawFd) -> io::Result<Termios> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let saved = termios::tcgetattr(&bfd).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let mut raw = saved.clone();
    termios::cfmakeraw(&mut raw);
    raw.control_chars[nix::sys::termios::SpecialCharacterIndices::VQUIT as usize] = 0;
    termios::tcsetattr(&bfd, SetArg::TCSAFLUSH, &raw)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    Ok(saved)
}

pub struct RawModeGuard {
    pub fd: RawFd,
    pub saved: Termios,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(&bfd, SetArg::TCSAFLUSH, &self.saved);
    }
}

struct NonBlockGuard {
    fd: RawFd,
}

impl Drop for NonBlockGuard {
    fn drop(&mut self) {
        use nix::fcntl::{fcntl, FcntlArg, OFlag};
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd) };
        if let Ok(fl) = fcntl(&bfd, FcntlArg::F_GETFL) {
            let fl = OFlag::from_bits_truncate(fl) & !OFlag::O_NONBLOCK;
            let _ = fcntl(&bfd, FcntlArg::F_SETFL(fl));
        }
    }
}

// ---------------------------------------------------------------------------
// DA query drain
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

// ---------------------------------------------------------------------------
// PTY spawning
// ---------------------------------------------------------------------------

pub fn spawn_pty(
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

    set_nonblock_and_cloexec(master_fd)?;
    Ok((master_fd, pid))
}

// ---------------------------------------------------------------------------
// Daemon
// ---------------------------------------------------------------------------

/// Per-client outgoing buffer cap. Above this, the client is dropped on the
/// next send to prevent unbounded growth from a stalled reader.
const MAX_OUT_BUF: usize = 4 * 1024 * 1024;

struct ClientConn {
    fd: OwnedFd,
    buf: SocketBuffer,
    out_buf: Vec<u8>,
}

impl ClientConn {
    /// Append a message to out_buf. Returns false if the buffer would exceed
    /// MAX_OUT_BUF — caller should remove the client.
    fn queue_send(&mut self, tag: Tag, data: &[u8]) -> bool {
        let total = ipc::HEADER_SIZE + data.len();
        if self.out_buf.len() + total > MAX_OUT_BUF {
            return false;
        }
        let header = ipc::encode_header(tag, data.len() as u32);
        self.out_buf.extend_from_slice(&header);
        self.out_buf.extend_from_slice(data);
        true
    }

    /// Drain out_buf via non-blocking write. Returns false on permanent error
    /// (caller should remove the client). EAGAIN leaves remainder queued.
    fn flush(&mut self) -> bool {
        let bfd = unsafe { BorrowedFd::borrow_raw(self.fd.as_raw_fd()) };
        while !self.out_buf.is_empty() {
            match unistd::write(&bfd, &self.out_buf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.out_buf.drain(..n);
                }
                Err(nix::errno::Errno::EAGAIN) => return true,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(_) => return false,
            }
        }
        true
    }

    fn wants_write(&self) -> bool {
        !self.out_buf.is_empty()
    }
}

pub struct Daemon {
    server_fd: RawFd,
    pty_master_fd: RawFd,
    child_pid: libc::pid_t,
    clients: Vec<ClientConn>,
    parser: vt100::Parser,
    session_name: String,
    socket_dir: PathBuf,
    shell_cmd: String,
    cwd: String,
    created_at: u64,
    task_ended_at: u64,
    task_exit_code: u8,
    child_exited: bool,
    has_had_client: bool,
    signal_read_fd: RawFd,
    last_client_disconnected_at: Option<u64>,
    empty_timeout: Option<u64>,
}

impl Daemon {
    fn broadcast(&mut self, tag: Tag, data: &[u8]) {
        let mut remove = Vec::new();
        for (i, client) in self.clients.iter_mut().enumerate() {
            if !client.queue_send(tag, data) {
                remove.push(i);
            }
        }
        for i in remove.into_iter().rev() {
            let c = self.clients.remove(i);
            log::info!(
                "client disconnected (out buffer full), fd={}",
                c.fd.as_raw_fd()
            );
        }
    }

    /// Opportunistically drain out_bufs after each event loop iteration so
    /// data goes out promptly without waiting for the next poll round.
    fn flush_clients(&mut self) {
        let mut remove = Vec::new();
        for (i, client) in self.clients.iter_mut().enumerate() {
            if client.wants_write() && !client.flush() {
                remove.push(i);
            }
        }
        for i in remove.into_iter().rev() {
            let c = self.clients.remove(i);
            log::info!("client disconnected (write error), fd={}", c.fd.as_raw_fd());
        }
    }

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

    fn send_info(&mut self, i: usize) {
        let payload = self.build_info().encode();
        let _ = self.clients[i].queue_send(Tag::Info, &payload);
    }

    fn handle_signal(&mut self) -> bool {
        let mut buf = [0u8; 64];
        let _ = read_raw(self.signal_read_fd, &mut buf);

        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(self.child_pid, &mut status, libc::WNOHANG) };
        if r > 0 {
            self.child_exited = true;
            if libc::WIFEXITED(status) {
                self.task_exit_code = libc::WEXITSTATUS(status) as u8;
            } else {
                self.task_exit_code = 1;
            }
            self.task_ended_at = now_epoch();
            log::info!(
                "child exited, pid={} exit_code={}",
                self.child_pid,
                self.task_exit_code
            );
        }
        false
    }

    fn handle_server(&mut self) {
        loop {
            let r =
                unsafe { libc::accept(self.server_fd, std::ptr::null_mut(), std::ptr::null_mut()) };
            if r < 0 {
                break;
            }
            let client_fd = unsafe { OwnedFd::from_raw_fd(r) };
            if let Err(e) = set_nonblock_and_cloexec(client_fd.as_raw_fd()) {
                log::warn!("failed to set flags on client fd: {}", e);
                continue;
            }
            log::info!("client connected, fd={}", client_fd.as_raw_fd());

            let mut client = ClientConn {
                fd: client_fd,
                buf: SocketBuffer::new(),
                out_buf: Vec::new(),
            };

            if self.has_had_client {
                if let Some(state) = util::serialize_terminal_state(&self.parser) {
                    let _ = client.queue_send(Tag::Init, &state);
                }
            }
            self.has_had_client = true;

            self.clients.push(client);
        }
    }

    fn handle_pty_output(&mut self) -> bool {
        let mut buf = [0u8; 4096];
        match read_raw(self.pty_master_fd, &mut buf) {
            Ok(0) => {
                log::info!("pty master returned EOF");
                return true;
            }
            Ok(n) => {
                let data = &buf[..n];
                self.parser.process(data);

                if let Some(code) = util::find_task_exit_marker(data) {
                    self.task_exit_code = code;
                    self.task_ended_at = now_epoch();
                    log::info!("task exit marker found, code={}", code);
                }

                self.broadcast(Tag::Output, data);

                if self.clients.is_empty() {
                    util::respond_to_device_attributes(self.pty_master_fd, data);
                }
            }
            Err(nix::errno::Errno::EIO) => {
                log::info!("pty master returned EIO (child exited)");
                return true;
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(e) => {
                log::warn!("pty read error: {}", e);
                return true;
            }
        }
        false
    }

    fn handle_client_data(&mut self, client_poll_fds: &[PollFd]) {
        let mut remove = Vec::new();
        for i in 0..self.clients.len() {
            if i >= client_poll_fds.len() {
                break;
            }
            let revents = match client_poll_fds[i].revents() {
                Some(r) => r,
                None => continue,
            };
            if revents.contains(PollFlags::POLLERR) {
                log::info!(
                    "client disconnected (poll error), fd={}",
                    self.clients[i].fd.as_raw_fd()
                );
                remove.push(i);
                continue;
            }
            if revents.contains(PollFlags::POLLOUT) && !self.clients[i].flush() {
                log::info!(
                    "client disconnected (write error), fd={}",
                    self.clients[i].fd.as_raw_fd()
                );
                remove.push(i);
                continue;
            }
            if !revents.contains(PollFlags::POLLIN) && !revents.contains(PollFlags::POLLHUP) {
                continue;
            }

            let client_fd = self.clients[i].fd.as_raw_fd();
            match self.clients[i].buf.read(client_fd) {
                Ok(0) => {
                    log::info!("client disconnected, fd={}", client_fd);
                    remove.push(i);
                    continue;
                }
                Ok(_) => {}
                Err(nix::errno::Errno::EAGAIN) => continue,
                Err(_) => {
                    remove.push(i);
                    continue;
                }
            }

            while let Some((tag, payload)) = self.clients[i].buf.next() {
                let payload = payload.to_vec();
                match tag {
                    Tag::Input => {
                        let _ = ipc::write_all(self.pty_master_fd, &payload);
                    }
                    Tag::Resize => {
                        if let Some(resize) = ipc::Resize::decode(&payload) {
                            self.parser.screen_mut().set_size(resize.rows, resize.cols);
                            let ws = libc::winsize {
                                ws_row: resize.rows,
                                ws_col: resize.cols,
                                ws_xpixel: 0,
                                ws_ypixel: 0,
                            };
                            unsafe {
                                libc::ioctl(self.pty_master_fd, libc::TIOCSWINSZ, &ws);
                            }
                        }
                    }
                    Tag::Detach => {
                        log::info!("client requested detach, fd={}", client_fd);
                        let _ = self.clients[i].queue_send(Tag::Detach, &[]);
                        remove.push(i);
                    }
                    Tag::DetachAll => {
                        log::info!("client requested detach-all");
                        for j in 0..self.clients.len() {
                            let _ = self.clients[j].queue_send(Tag::Detach, &[]);
                        }
                        remove.clear();
                        for j in 0..self.clients.len() {
                            remove.push(j);
                        }
                        break;
                    }
                    Tag::Kill => {
                        log::info!("kill requested");
                        unsafe {
                            libc::kill(self.child_pid, libc::SIGTERM);
                        }
                    }
                    Tag::Info => {
                        self.send_info(i);
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
                        let data =
                            util::serialize_terminal(&self.parser, format).unwrap_or_default();
                        let _ = self.clients[i].queue_send(Tag::History, &data);
                    }
                    Tag::Print => {
                        if !payload.is_empty() {
                            self.parser.process(&payload);
                            self.broadcast(Tag::Output, &payload);
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
                    _ => {}
                }
            }
        }

        remove.sort_unstable();
        remove.dedup();
        for i in remove.into_iter().rev() {
            self.clients.remove(i);
        }
    }
}

fn daemon_loop(daemon: &mut Daemon) {
    loop {
        let sig_bfd = unsafe { BorrowedFd::borrow_raw(daemon.signal_read_fd) };
        let srv_bfd = unsafe { BorrowedFd::borrow_raw(daemon.server_fd) };
        let pty_bfd = unsafe { BorrowedFd::borrow_raw(daemon.pty_master_fd) };

        let mut poll_fds = vec![
            PollFd::new(sig_bfd, PollFlags::POLLIN),
            PollFd::new(srv_bfd, PollFlags::POLLIN),
            PollFd::new(pty_bfd, PollFlags::POLLIN),
        ];

        for client in &daemon.clients {
            let bfd = unsafe { BorrowedFd::borrow_raw(client.fd.as_raw_fd()) };
            let flags = if client.wants_write() {
                PollFlags::POLLIN | PollFlags::POLLOUT
            } else {
                PollFlags::POLLIN
            };
            poll_fds.push(PollFd::new(bfd, flags));
        }

        let mut poll_timeout = PollTimeout::NONE;
        if let (Some(dis_at), Some(limit)) =
            (daemon.last_client_disconnected_at, daemon.empty_timeout)
        {
            let elapsed = now_epoch().saturating_sub(dis_at);
            if elapsed >= limit {
                log::info!(
                    "empty session timeout of {}s reached, self-terminating",
                    limit
                );
                break;
            } else {
                let remaining_secs = limit - elapsed;
                let remaining_ms = std::cmp::min(remaining_secs, 30) * 1000;
                poll_timeout = PollTimeout::from(remaining_ms as u16);
            }
        }

        match poll(&mut poll_fds, poll_timeout) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => {
                log::error!("poll error: {}", e);
                break;
            }
        }

        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                daemon.handle_signal();
            }
        }

        if let Some(revents) = poll_fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                daemon.handle_server();
            }
        }

        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) || revents.contains(PollFlags::POLLHUP) {
                if daemon.handle_pty_output() {
                    break;
                }
            }
        }

        daemon.handle_client_data(&poll_fds[3..]);
        daemon.flush_clients();

        if daemon.clients.is_empty() {
            if daemon.has_had_client && daemon.last_client_disconnected_at.is_none() {
                daemon.last_client_disconnected_at = Some(now_epoch());
            }
        } else {
            daemon.last_client_disconnected_at = None;
        }

        if daemon.child_exited {
            let _ = daemon.handle_pty_output();
            break;
        }
    }
}

pub fn run_daemon(cfg: &Cfg, server_fd: RawFd, cmd: &[String]) {
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
    let (master_fd, child_pid) = match spawn_pty(spawn_cmd, &spawn_args, 24, 80, &cfg.session_name)
    {
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

    let (sig_read, _sig_write) = match create_signal_pipe() {
        Ok(fds) => fds,
        Err(e) => {
            log::error!("failed to create signal pipe: {}", e);
            return;
        }
    };

    install_signal_handler(Signal::SIGCHLD);
    install_signal_handler(Signal::SIGTERM);

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let display_cmd = if cmd.is_empty() {
        shell.clone()
    } else {
        cmd.join(" ")
    };
    log::info!("child spawned, pid={} cmd={}", child_pid, display_cmd);

    let mut daemon = Daemon {
        server_fd,
        pty_master_fd: master_fd,
        child_pid,
        clients: Vec::new(),
        parser: vt100::Parser::new(24, 80, 1000),
        session_name: cfg.session_name.clone(),
        socket_dir: cfg.socket_dir.clone(),
        shell_cmd: display_cmd,
        cwd,
        created_at: now_epoch(),
        task_ended_at: 0,
        task_exit_code: 0,
        child_exited: false,
        has_had_client: false,
        signal_read_fd: sig_read,
        last_client_disconnected_at: None,
        empty_timeout,
    };

    if !early_output.is_empty() {
        daemon.parser.process(&early_output);
    }

    daemon_loop(&mut daemon);

    log::info!("daemon exiting, session={}", daemon.session_name);

    for c in daemon.clients.iter_mut() {
        let _ = c.queue_send(Tag::Detach, &[]);
        let _ = c.flush();
    }
    daemon.clients.clear();

    unsafe {
        libc::close(daemon.pty_master_fd);
        libc::close(daemon.server_fd);
    }

    let _ = std::fs::remove_file(&cfg.socket_path);
    let symlink_path = cfg
        .socket_dir
        .join(format!("{}.ssh-auth-sock", cfg.session_name));
    let _ = std::fs::remove_file(symlink_path);
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

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub fn run_client(socket: OwnedFd) -> i32 {
    let socket_fd = socket.as_raw_fd();
    let stdin_fd: RawFd = 0;
    let stdout_fd: RawFd = 1;

    if let Err(e) = set_nonblock_and_cloexec(socket_fd) {
        eprintln!("error: failed to set socket nonblock: {}", e);
        return 1;
    }

    if let Err(e) = set_nonblock_and_cloexec(stdout_fd) {
        eprintln!("error: failed to set stdout nonblock: {}", e);
        return 1;
    }
    let _stdout_guard = NonBlockGuard { fd: stdout_fd };

    let saved = match enter_raw_mode(stdin_fd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to enter raw mode: {}", e);
            return 1;
        }
    };
    let _guard = RawModeGuard {
        fd: stdin_fd,
        saved,
    };

    ignore_signal(Signal::SIGPIPE);
    let (sig_read, _sig_write) = match create_signal_pipe() {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("error: failed to create signal pipe: {}", e);
            return 1;
        }
    };
    install_signal_handler(Signal::SIGWINCH);

    let size = ipc::get_terminal_size(stdout_fd);
    let _ = ipc::send(socket_fd, Tag::Resize, &size.encode());

    if let Ok(ssh_auth_sock) = std::env::var("SSH_AUTH_SOCK") {
        let _ = ipc::send(socket_fd, Tag::SshAuthSock, ssh_auth_sock.as_bytes());
    }

    client_loop(socket_fd, sig_read, stdin_fd, stdout_fd)
}

fn drain_output(fd: RawFd, buf: &mut Vec<u8>) {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    while !buf.is_empty() {
        match unistd::write(&bfd, buf) {
            Ok(n) if n > 0 => {
                buf.drain(..n);
            }
            Err(nix::errno::Errno::EINTR) => continue,
            _ => break,
        }
    }
}

fn client_loop(socket_fd: RawFd, signal_fd: RawFd, stdin_fd: RawFd, stdout_fd: RawFd) -> i32 {
    let mut socket_buf = SocketBuffer::new();
    let mut out_buf: Vec<u8> = Vec::new();
    let mut exit_code: i32 = 0;
    let mut detached = false;
    const MAX_OUT_BUF: usize = 4 * 1024 * 1024;

    loop {
        let stdin_bfd = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(socket_fd) };
        let sig_bfd = unsafe { BorrowedFd::borrow_raw(signal_fd) };

        let has_pending_output = !out_buf.is_empty();
        let mut poll_fds = vec![
            PollFd::new(stdin_bfd, PollFlags::POLLIN),
            PollFd::new(sock_bfd, PollFlags::POLLIN),
            PollFd::new(sig_bfd, PollFlags::POLLIN),
        ];
        if has_pending_output {
            let out_bfd = unsafe { BorrowedFd::borrow_raw(stdout_fd) };
            poll_fds.push(PollFd::new(out_bfd, PollFlags::POLLOUT));
        }

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        if has_pending_output {
            if let Some(revents) = poll_fds[3].revents() {
                if revents.contains(PollFlags::POLLOUT) {
                    drain_output(stdout_fd, &mut out_buf);
                }
            }
        }

        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 64];
                let _ = read_raw(signal_fd, &mut buf);
                let size = ipc::get_terminal_size(stdout_fd);
                let _ = ipc::send(socket_fd, Tag::Resize, &size.encode());
            }
        }

        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match read_raw(stdin_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];
                        if data.contains(&0x1c) || util::is_kitty_ctrl_backslash(data) {
                            let _ = ipc::send(socket_fd, Tag::Detach, &[]);
                            break;
                        }
                        let _ = ipc::send(socket_fd, Tag::Input, data);
                    }
                    Err(nix::errno::Errno::EAGAIN) => {}
                    Err(_) => break,
                }
            }
        }

        if let Some(revents) = poll_fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) || revents.contains(PollFlags::POLLHUP) {
                match socket_buf.read(socket_fd) {
                    Ok(0) => {
                        break;
                    }
                    Ok(_) => {
                        while let Some((tag, payload)) = socket_buf.next() {
                            match tag {
                                Tag::Output | Tag::Init => {
                                    if out_buf.len() + payload.len() > MAX_OUT_BUF {
                                        let excess = out_buf.len() + payload.len() - MAX_OUT_BUF;
                                        out_buf.drain(..excess.min(out_buf.len()));
                                    }
                                    out_buf.extend_from_slice(&payload);
                                }
                                Tag::Detach => {
                                    detached = true;
                                    break;
                                }
                                Tag::Ack => {
                                    if !payload.is_empty() {
                                        exit_code = payload[0] as i32;
                                    }
                                    drain_output(stdout_fd, &mut out_buf);
                                    return exit_code;
                                }
                                _ => {}
                            }
                        }
                        drain_output(stdout_fd, &mut out_buf);
                    }
                    Err(nix::errno::Errno::EAGAIN) => {}
                    Err(_) => break,
                }
            }
        }

        if detached {
            break;
        }
    }

    drain_output(stdout_fd, &mut out_buf);
    exit_code
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
