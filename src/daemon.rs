use std::io;
use std::os::unix::io::{BorrowedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigAction, SigHandler, SaFlags, SigSet, Signal, sigaction};
use nix::sys::termios::{self, Termios, SetArg};
use nix::unistd;

use crate::ipc::{self, Tag, SocketBuffer};
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
        let session_name = socket::get_session_name(&prefix, name)
            .map_err(|e| format!("{}", e))?;
        let socket_dir = socket::socket_dir();
        let socket_path = socket::get_socket_path(&socket_dir, &session_name)
            .map_err(|_| {
                socket::print_session_name_too_long(&session_name, &socket_dir);
                "socket path too long".to_string()
            })?;
        Ok(Cfg { session_name, socket_dir, socket_path })
    }
}

// ---------------------------------------------------------------------------
// Raw helpers
// ---------------------------------------------------------------------------

pub fn size_as_bytes(r: &ipc::Resize) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            r as *const ipc::Resize as *const u8,
            std::mem::size_of::<ipc::Resize>(),
        )
    }
}

pub fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    unistd::read(&bfd, buf)
}

pub fn write_all_raw(fd: RawFd, data: &[u8]) -> io::Result<()> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut offset = 0;
    while offset < data.len() {
        match unistd::write(&bfd, &data[offset..]) {
            Ok(n) => {
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
                }
                offset += n;
            }
            Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
    Ok(())
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
    use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };

    let fl = fcntl(&bfd, FcntlArg::F_GETFL)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let fl = OFlag::from_bits_truncate(fl) | OFlag::O_NONBLOCK;
    fcntl(&bfd, FcntlArg::F_SETFL(fl))
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

    let fd_flags = fcntl(&bfd, FcntlArg::F_GETFD)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let fd_flags = FdFlag::from_bits_truncate(fd_flags) | FdFlag::FD_CLOEXEC;
    fcntl(&bfd, FcntlArg::F_SETFD(fd_flags))
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

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
    unsafe { let _ = sigaction(sig, &sa); }
}

pub fn ignore_signal(sig: Signal) {
    let sa = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { let _ = sigaction(sig, &sa); }
}

// ---------------------------------------------------------------------------
// Terminal raw mode
// ---------------------------------------------------------------------------

pub fn enter_raw_mode(fd: RawFd) -> io::Result<Termios> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let saved = termios::tcgetattr(&bfd)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
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

// ---------------------------------------------------------------------------
// DA query drain
// ---------------------------------------------------------------------------

fn drain_da_queries(master_fd: RawFd) -> Vec<u8> {
    let bfd = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
    let mut buf = [0u8; 4096];
    let mut collected = Vec::new();

    for _ in 0..200 {
        match poll(&mut poll_fds, PollTimeout::from(10u16)) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(_) => continue,
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

pub fn spawn_pty(cmd: &str, args: &[&str], rows: u16, cols: u16, session_name: &str) -> io::Result<(RawFd, libc::pid_t)> {
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
            let key = std::ffi::CString::new("RIF_SESSION").unwrap();
            let val = std::ffi::CString::new(session_name).unwrap();
            libc::setenv(key.as_ptr(), val.as_ptr(), 1);

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
                let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
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

struct ClientConn {
    fd: RawFd,
    buf: SocketBuffer,
}

pub struct Daemon {
    server_fd: RawFd,
    pty_master_fd: RawFd,
    child_pid: libc::pid_t,
    clients: Vec<ClientConn>,
    parser: vt100::Parser,
    session_name: String,
    shell_cmd: String,
    cwd: String,
    created_at: u64,
    task_ended_at: u64,
    task_exit_code: u8,
    child_exited: bool,
    has_had_client: bool,
    signal_read_fd: RawFd,
}

impl Daemon {
    fn broadcast(&mut self, tag: Tag, data: &[u8]) {
        let mut remove = Vec::new();
        for (i, client) in self.clients.iter().enumerate() {
            if ipc::send(client.fd, tag, data).is_err() {
                remove.push(i);
            }
        }
        for i in remove.into_iter().rev() {
            let c = self.clients.remove(i);
            unsafe { libc::close(c.fd); }
            log::info!("client disconnected (write error), fd={}", c.fd);
        }
    }

    fn build_info(&self) -> ipc::Info {
        let mut info: ipc::Info = unsafe { std::mem::zeroed() };
        info.clients_len = self.clients.len();
        info.pid = self.child_pid;
        info.created_at = self.created_at;
        info.task_ended_at = self.task_ended_at;
        info.task_exit_code = self.task_exit_code;

        let cmd_bytes = self.shell_cmd.as_bytes();
        let cmd_len = cmd_bytes.len().min(ipc::MAX_CMD_LEN);
        info.cmd[..cmd_len].copy_from_slice(&cmd_bytes[..cmd_len]);
        info.cmd_len = cmd_len as u16;

        let cwd_bytes = self.cwd.as_bytes();
        let cwd_len = cwd_bytes.len().min(ipc::MAX_CWD_LEN);
        info.cwd[..cwd_len].copy_from_slice(&cwd_bytes[..cwd_len]);
        info.cwd_len = cwd_len as u16;

        info
    }

    fn send_info(&self, fd: RawFd) {
        let info = self.build_info();
        let data = unsafe {
            std::slice::from_raw_parts(
                &info as *const ipc::Info as *const u8,
                std::mem::size_of::<ipc::Info>(),
            )
        };
        let _ = ipc::send(fd, Tag::Info, data);
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
            log::info!("child exited, pid={} exit_code={}", self.child_pid, self.task_exit_code);
        }
        false
    }

    fn handle_server(&mut self) {
        loop {
            let r = unsafe {
                libc::accept(self.server_fd, std::ptr::null_mut(), std::ptr::null_mut())
            };
            if r < 0 {
                break;
            }
            let client_fd = r;
            if let Err(e) = set_nonblock_and_cloexec(client_fd) {
                log::warn!("failed to set flags on client fd: {}", e);
                unsafe { libc::close(client_fd); }
                continue;
            }
            log::info!("client connected, fd={}", client_fd);

            if self.has_had_client {
                if let Some(state) = util::serialize_terminal_state(&self.parser) {
                    let _ = ipc::send(client_fd, Tag::Init, &state);
                }
            }
            self.has_had_client = true;

            self.clients.push(ClientConn {
                fd: client_fd,
                buf: SocketBuffer::new(),
            });
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
                log::info!("client disconnected (poll error), fd={}", self.clients[i].fd);
                remove.push(i);
                continue;
            }
            if !revents.contains(PollFlags::POLLIN) && !revents.contains(PollFlags::POLLHUP) {
                continue;
            }

            let client_fd = self.clients[i].fd;
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
                match tag {
                    Tag::Input => {
                        let _ = write_all_raw(self.pty_master_fd, &payload);
                    }
                    Tag::Resize => {
                        if payload.len() == std::mem::size_of::<ipc::Resize>() {
                            let resize: ipc::Resize = unsafe {
                                std::ptr::read(payload.as_ptr() as *const ipc::Resize)
                            };
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
                        let _ = ipc::send(client_fd, Tag::Detach, &[]);
                        remove.push(i);
                    }
                    Tag::DetachAll => {
                        log::info!("client requested detach-all");
                        for j in 0..self.clients.len() {
                            let _ = ipc::send(self.clients[j].fd, Tag::Detach, &[]);
                        }
                        remove.clear();
                        for j in 0..self.clients.len() {
                            remove.push(j);
                        }
                        break;
                    }
                    Tag::Kill => {
                        log::info!("kill requested");
                        unsafe { libc::kill(self.child_pid, libc::SIGTERM); }
                    }
                    Tag::Info => {
                        self.send_info(client_fd);
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
                        if let Some(data) = util::serialize_terminal(&self.parser, format) {
                            let _ = ipc::send(client_fd, Tag::History, &data);
                        } else {
                            let _ = ipc::send(client_fd, Tag::History, &[]);
                        }
                    }
                    Tag::Print => {
                        if !payload.is_empty() {
                            self.parser.process(&payload);
                            self.broadcast(Tag::Output, &payload);
                        }
                    }
                    Tag::Run => {
                        if !payload.is_empty() {
                            let _ = write_all_raw(self.pty_master_fd, &payload);
                        }
                    }
                    _ => {}
                }
            }
        }

        remove.sort_unstable();
        remove.dedup();
        for i in remove.into_iter().rev() {
            let c = self.clients.remove(i);
            unsafe { libc::close(c.fd); }
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
            let bfd = unsafe { BorrowedFd::borrow_raw(client.fd) };
            poll_fds.push(PollFd::new(bfd, PollFlags::POLLIN));
        }

        match poll(&mut poll_fds, PollTimeout::NONE) {
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

        if daemon.child_exited {
            let _ = daemon.handle_pty_output();
            break;
        }
    }
}

pub fn run_daemon(cfg: &Cfg, server_fd: RawFd, cmd: &[String]) {
    ignore_signal(Signal::SIGPIPE);

    let shell = util::detect_shell();
    let spawn_cmd = if cmd.is_empty() { &shell } else { &cmd[0] };
    let spawn_args: Vec<&str> = if cmd.is_empty() { vec![] } else { cmd[1..].iter().map(|s| s.as_str()).collect() };
    let (master_fd, child_pid) = match spawn_pty(spawn_cmd, &spawn_args, 24, 80, &cfg.session_name) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: failed to spawn pty: {}", e);
            let _ = std::fs::remove_file(&cfg.socket_path);
            return;
        }
    };

    let early_output = drain_da_queries(master_fd);

    let log_system = Box::leak(Box::new(crate::logger::LogSystem::new()));
    let log_path = cfg.socket_dir.join("logs").join(format!("{}.log", cfg.session_name));
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
        shell_cmd: display_cmd,
        cwd,
        created_at: now_epoch(),
        task_ended_at: 0,
        task_exit_code: 0,
        child_exited: false,
        has_had_client: false,
        signal_read_fd: sig_read,
    };

    if !early_output.is_empty() {
        daemon.parser.process(&early_output);
    }

    daemon_loop(&mut daemon);

    log::info!("daemon exiting, session={}", daemon.session_name);

    for c in &daemon.clients {
        let _ = ipc::send(c.fd, Tag::Detach, &[]);
        unsafe { libc::close(c.fd); }
    }

    unsafe {
        libc::close(daemon.pty_master_fd);
        libc::close(daemon.server_fd);
    }

    let _ = std::fs::remove_file(&cfg.socket_path);
}

pub fn spawn_daemon(cfg: &Cfg, cmd: &[String]) -> Result<RawFd, String> {
    let server_fd = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;

    let cmd_owned: Vec<String> = cmd.to_vec();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(server_fd); }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        unsafe { libc::setsid(); }
        redirect_std_to_devnull();
        run_daemon(cfg, server_fd, &cmd_owned);
        unsafe { libc::_exit(0); }
    }

    unsafe { libc::close(server_fd); }
    std::thread::sleep(std::time::Duration::from_millis(10));

    let path_str = cfg.socket_path.to_str()
        .ok_or("invalid socket path")?;

    for i in 0..10 {
        match socket::session_connect(path_str) {
            Ok(fd) => return Ok(fd),
            Err(_) if i < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(format!("failed to connect to new session: {}", e)),
        }
    }
    unreachable!()
}

pub fn spawn_daemon_detached(cfg: &Cfg, cmd: &[String]) -> Result<(), String> {
    let server_fd = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;

    let cmd_owned: Vec<String> = cmd.to_vec();
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(server_fd); }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        unsafe { libc::setsid(); }
        redirect_std_to_devnull();
        run_daemon(cfg, server_fd, &cmd_owned);
        unsafe { libc::_exit(0); }
    }

    unsafe { libc::close(server_fd); }
    println!("session '{}' created", cfg.session_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub fn run_client(socket_fd: RawFd) -> i32 {
    let stdin_fd: RawFd = 0;
    let stdout_fd: RawFd = 1;

    let saved = match enter_raw_mode(stdin_fd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to enter raw mode: {}", e);
            return 1;
        }
    };
    let _guard = RawModeGuard { fd: stdin_fd, saved };

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
    let _ = ipc::send(socket_fd, Tag::Resize, size_as_bytes(&size));

    client_loop(socket_fd, sig_read, stdin_fd, stdout_fd)
}

fn client_loop(socket_fd: RawFd, signal_fd: RawFd, stdin_fd: RawFd, stdout_fd: RawFd) -> i32 {
    let mut socket_buf = SocketBuffer::new();
    let mut exit_code: i32 = 0;
    let mut detached = false;

    loop {
        let stdin_bfd = unsafe { BorrowedFd::borrow_raw(stdin_fd) };
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(socket_fd) };
        let sig_bfd = unsafe { BorrowedFd::borrow_raw(signal_fd) };

        let mut poll_fds = [
            PollFd::new(stdin_bfd, PollFlags::POLLIN),
            PollFd::new(sock_bfd, PollFlags::POLLIN),
            PollFd::new(sig_bfd, PollFlags::POLLIN),
        ];

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 64];
                let _ = read_raw(signal_fd, &mut buf);
                let size = ipc::get_terminal_size(stdout_fd);
                let _ = ipc::send(socket_fd, Tag::Resize, size_as_bytes(&size));
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
                                    let _ = write_all_raw(stdout_fd, &payload);
                                }
                                Tag::Detach => {
                                    detached = true;
                                    break;
                                }
                                Tag::Ack => {
                                    if !payload.is_empty() {
                                        exit_code = payload[0] as i32;
                                    }
                                    return exit_code;
                                }
                                _ => {}
                            }
                        }
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

    exit_code
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
