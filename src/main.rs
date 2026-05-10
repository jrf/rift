mod completions;
mod ipc;
mod logger;
mod socket;
mod util;

use std::io;
use std::os::unix::io::{BorrowedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::{SigAction, SigHandler, SaFlags, SigSet, Signal, sigaction};
use nix::sys::termios::{self, Termios, SetArg};
use nix::unistd;

use crate::ipc::{Tag, SocketBuffer};

// ---------------------------------------------------------------------------
// Cfg
// ---------------------------------------------------------------------------

struct Cfg {
    session_name: String,
    socket_dir: PathBuf,
    socket_path: PathBuf,
}

impl Cfg {
    fn resolve(name: &str) -> Result<Self, String> {
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
// CLI parsing
// ---------------------------------------------------------------------------

enum Command {
    Attach { name: String, detached: bool },
    List { short: bool },
    Run { name: String, cmd: Vec<String> },
    Kill { name: String },
    Detach { name: String },
    History { name: String, format: util::HistoryFormat },
    Wait { names: Vec<String> },
    Completions { shell: String },
    Version,
    Help,
}

fn parse_args() -> Command {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        return Command::Help;
    }

    let first = args[0].as_str();
    match first {
        "--help" | "-h" | "help" => Command::Help,
        "--version" | "-V" | "version" => Command::Version,
        "list" | "ls" => {
            let short = args.iter().any(|a| a == "-s" || a == "--short");
            Command::List { short }
        }
        "kill" => {
            if args.len() < 2 {
                eprintln!("error: kill requires a session name");
                std::process::exit(1);
            }
            Command::Kill { name: args[1].clone() }
        }
        "detach" | "d" => {
            let name = if args.len() >= 2 {
                args[1].clone()
            } else {
                let env = socket::session_name_from_env();
                if env.is_empty() {
                    eprintln!("error: detach requires a session name");
                    std::process::exit(1);
                }
                env
            };
            Command::Detach { name }
        }
        "run" => {
            if args.len() < 2 {
                eprintln!("error: run requires a session name");
                std::process::exit(1);
            }
            let name = args[1].clone();
            let cmd = args[2..].to_vec();
            Command::Run { name, cmd }
        }
        "history" | "hi" => {
            let mut session_name: Option<String> = None;
            let mut format = util::HistoryFormat::Plain;
            for arg in &args[1..] {
                match arg.as_str() {
                    "--vt" => format = util::HistoryFormat::Vt,
                    "--html" => format = util::HistoryFormat::Html,
                    _ if session_name.is_none() => session_name = Some(arg.clone()),
                    _ => {}
                }
            }
            let name = session_name.unwrap_or_else(|| socket::session_name_from_env());
            if name.is_empty() {
                eprintln!("error: history requires a session name");
                std::process::exit(1);
            }
            Command::History { name, format }
        }
        "wait" | "w" => {
            let names: Vec<String> = args[1..].to_vec();
            Command::Wait { names }
        }
        "completions" => {
            if args.len() < 2 {
                eprintln!("error: completions requires a shell name (bash, zsh, fish)");
                std::process::exit(1);
            }
            Command::Completions { shell: args[1].clone() }
        }
        "new" => {
            if args.len() < 2 {
                eprintln!("error: new requires a session name");
                std::process::exit(1);
            }
            Command::Attach { name: args[1].clone(), detached: true }
        }
        "attach" => {
            if args.len() < 2 {
                eprintln!("error: attach requires a session name");
                std::process::exit(1);
            }
            let detached = args.iter().any(|a| a == "-d" || a == "--detached");
            let name = args[1..].iter()
                .find(|a| !a.starts_with('-'))
                .cloned()
                .unwrap_or_else(|| { eprintln!("error: attach requires a session name"); std::process::exit(1); });
            Command::Attach { name, detached }
        }
        name => {
            // Default: treat bare argument as attach
            if name.starts_with('-') {
                eprintln!("error: unknown option '{}'", name);
                std::process::exit(1);
            }
            Command::Attach { name: name.to_string(), detached: false }
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() {
    let cmd = parse_args();
    let code = match cmd {
        Command::Help => { print_help(); 0 }
        Command::Version => { println!("ryx {}", env!("CARGO_PKG_VERSION")); 0 }
        Command::List { short } => cmd_list(short),
        Command::Kill { name } => cmd_kill(&name),
        Command::Detach { name } => cmd_detach(&name),
        Command::Run { name, cmd } => cmd_run(&name, &cmd),
        Command::History { name, format } => cmd_history(&name, format),
        Command::Wait { names } => cmd_wait(&names),
        Command::Completions { shell } => { completions::print_completions(&shell); 0 }
        Command::Attach { name, detached } => cmd_attach(&name, detached),
    };
    std::process::exit(code);
}

fn print_help() {
    println!(
        "\
ryx — terminal session daemon

Usage:
  ryx <session>              Attach to (or create) a session
  ryx attach <session>       Same as above
  ryx attach -d <session>    Create session without attaching
  ryx new <session>          Same as attach -d
  ryx list [-s]              List sessions (-s for short format)
  ryx run <session> <cmd...> Run a command in a session
  ryx history <session>      Print session output (--vt, --html)
  ryx detach <session>       Detach all clients from a session
  ryx kill <session>         Kill a session
  ryx wait <name>...         Wait for sessions to complete
  ryx completions <shell>    Print shell completions (bash, zsh, fish)
  ryx version                Print version
  ryx help                   Print this help

Detach key: Ctrl+\\"
    );
}

// ---------------------------------------------------------------------------
// DA query drain — respond to shell DA queries as fast as possible
// ---------------------------------------------------------------------------

/// Poll the PTY master briefly and respond to any DA queries.
/// Called immediately after spawning the PTY, before any other setup,
/// to ensure the response reaches the shell within its timeout window.
/// Returns any data read so it can be fed to the parser later.
fn drain_da_queries(master_fd: RawFd) -> Vec<u8> {
    let bfd = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
    let mut buf = [0u8; 4096];
    let mut collected = Vec::new();

    // Poll for up to 2 seconds in short intervals to catch DA queries
    for _ in 0..200 {
        match poll(&mut poll_fds, PollTimeout::from(10u16)) {
            Ok(0) => continue, // no data yet
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
// Raw helpers
// ---------------------------------------------------------------------------

fn size_as_bytes(r: &ipc::Resize) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(
            r as *const ipc::Resize as *const u8,
            std::mem::size_of::<ipc::Resize>(),
        )
    }
}

fn read_raw(fd: RawFd, buf: &mut [u8]) -> nix::Result<usize> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    unistd::read(&bfd, buf)
}

fn write_all_raw(fd: RawFd, data: &[u8]) -> io::Result<()> {
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

fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<()> {
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

fn create_signal_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    set_nonblock_and_cloexec(fds[0])?;
    set_nonblock_and_cloexec(fds[1])?;
    SIGNAL_FD.store(fds[1], Ordering::Relaxed);
    Ok((fds[0], fds[1]))
}

fn install_signal_handler(sig: Signal) {
    let sa = SigAction::new(
        SigHandler::Handler(signal_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe { let _ = sigaction(sig, &sa); }
}

fn ignore_signal(sig: Signal) {
    let sa = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe { let _ = sigaction(sig, &sa); }
}

// ---------------------------------------------------------------------------
// Terminal raw mode
// ---------------------------------------------------------------------------

fn enter_raw_mode(fd: RawFd) -> io::Result<Termios> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let saved = termios::tcgetattr(&bfd)
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    let mut raw = saved.clone();
    termios::cfmakeraw(&mut raw);
    // Disable SIGQUIT so Ctrl+\ can be used as detach key
    raw.control_chars[nix::sys::termios::SpecialCharacterIndices::VQUIT as usize] = 0;
    termios::tcsetattr(&bfd, SetArg::TCSAFLUSH, &raw)
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
        let _ = termios::tcsetattr(&bfd, SetArg::TCSAFLUSH, &self.saved);
    }
}

// ---------------------------------------------------------------------------
// Simple subcommands
// ---------------------------------------------------------------------------

fn cmd_list(short: bool) -> i32 {
    let socket_dir = socket::socket_dir();
    let current = socket::session_name_from_env();
    let current_ref = if current.is_empty() { None } else { Some(current.as_str()) };

    match util::get_session_entries(&socket_dir) {
        Ok(entries) => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for entry in &entries {
                let _ = util::write_session_line(&mut out, entry, short, current_ref);
            }
            0
        }
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                // No sessions directory yet — nothing to list
                0
            } else {
                eprintln!("error: {}", e);
                1
            }
        }
    }
}

fn cmd_kill(name: &str) -> i32 {
    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };
    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s,
        None => { eprintln!("error: invalid socket path"); return 1; }
    };

    // Probe first to get the pid for fallback SIGTERM
    let pid = match ipc::probe_session(path_str) {
        Ok(result) => {
            let pid = result.info.pid;
            // Reuse the probe connection to send kill
            let _ = ipc::send(result.fd, Tag::Kill, &[]);
            unsafe { libc::close(result.fd); }
            Some(pid)
        }
        Err(_) => {
            // Probe failed — try a plain connect + kill anyway
            match socket::session_connect(path_str) {
                Ok(fd) => {
                    let _ = ipc::send(fd, Tag::Kill, &[]);
                    unsafe { libc::close(fd); }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::ConnectionRefused {
                        // Dead socket, just clean it up
                        socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                        return 0;
                    }
                    eprintln!("error: cannot connect to session '{}': {}", name, e);
                    return 1;
                }
            }
            None
        }
    };

    // Wait for socket to disappear
    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
            Ok(false) => return 0,
            _ => {}
        }
    }

    // Socket still exists — fall back to SIGTERM if we have a pid
    if let Some(pid) = pid {
        unsafe { libc::kill(pid, libc::SIGTERM); }
        // Wait again for cleanup
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
                Ok(false) => return 0,
                _ => {}
            }
        }
    }

    // Last resort — remove the socket manually
    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
    0
}

fn cmd_detach(name: &str) -> i32 {
    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };
    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s,
        None => { eprintln!("error: invalid socket path"); return 1; }
    };
    let fd = match socket::session_connect(path_str) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("error: cannot connect to session '{}': {}", name, e); return 1; }
    };
    if let Err(e) = ipc::send(fd, Tag::DetachAll, &[]) {
        eprintln!("error: failed to send detach: {}", e);
        unsafe { libc::close(fd); }
        return 1;
    }
    unsafe { libc::close(fd); }
    0
}

// ---------------------------------------------------------------------------
// PTY spawning
// ---------------------------------------------------------------------------

fn spawn_pty(shell: &str, rows: u16, cols: u16, session_name: &str) -> io::Result<(RawFd, libc::pid_t)> {
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
        // Child process
        unsafe {
            // Set RYX_SESSION env var
            let key = std::ffi::CString::new("RYX_SESSION").unwrap();
            let val = std::ffi::CString::new(session_name).unwrap();
            libc::setenv(key.as_ptr(), val.as_ptr(), 1);

            // Reset signal handlers
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);

            let shell_cstr = std::ffi::CString::new(shell).unwrap();
            let login_name = format!("-{}", shell.rsplit('/').next().unwrap_or(shell));
            let login_cstr = std::ffi::CString::new(login_name).unwrap();

            libc::execl(
                shell_cstr.as_ptr(),
                login_cstr.as_ptr(),
                std::ptr::null::<libc::c_char>(),
            );

            // If exec fails, _exit immediately — never return from child
            libc::_exit(127);
        }
    }

    // Parent
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

struct Daemon {
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

        // Check for SIGCHLD — child exited
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
            // Don't exit yet — drain remaining PTY output first
        }
        false
    }

    fn handle_server(&mut self) {
        loop {
            let r = unsafe {
                libc::accept(self.server_fd, std::ptr::null_mut(), std::ptr::null_mut())
            };
            if r < 0 {
                break; // EAGAIN or error
            }
            let client_fd = r;
            if let Err(e) = set_nonblock_and_cloexec(client_fd) {
                log::warn!("failed to set flags on client fd: {}", e);
                unsafe { libc::close(client_fd); }
                continue;
            }
            log::info!("client connected, fd={}", client_fd);

            // Send terminal state on re-attach, but skip on first attach
            // to avoid interfering with shell initialization (DA queries, etc.)
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

                // Check for task exit marker
                if let Some(code) = util::find_task_exit_marker(data) {
                    self.task_exit_code = code;
                    self.task_ended_at = now_epoch();
                    log::info!("task exit marker found, code={}", code);
                }

                // Broadcast output to clients
                self.broadcast(Tag::Output, data);

                // When no clients are attached, respond to DA queries
                // so shells like fish don't timeout waiting
                if self.clients.is_empty() {
                    util::respond_to_device_attributes(self.pty_master_fd, data);
                }
            }
            Err(nix::errno::Errno::EIO) => {
                // EIO on PTY master means child exited
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
                            // ioctl to resize PTY
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
                        // Detach all clients
                        for j in 0..self.clients.len() {
                            let _ = ipc::send(self.clients[j].fd, Tag::Detach, &[]);
                        }
                        // Mark all for removal
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
                    Tag::Run => {
                        // Inject command into PTY
                        if !payload.is_empty() {
                            let cmd = String::from_utf8_lossy(&payload);
                            // Wrap with task marker for exit code tracking
                            let wrapped = format!(
                                "{}; printf 'RYX_TASK_COMPLETED:%d' $?\n",
                                cmd
                            );
                            let _ = write_all_raw(self.pty_master_fd, wrapped.as_bytes());
                        }
                    }
                    _ => {}
                }
            }
        }

        // Remove disconnected clients in reverse order
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
        // Build poll fds: [signal_pipe, server_fd, pty_master, ...clients]
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

        // Signal pipe
        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                daemon.handle_signal();
            }
        }

        // Server socket — accept new clients
        if let Some(revents) = poll_fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) {
                daemon.handle_server();
            }
        }

        // PTY master — output from child
        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) || revents.contains(PollFlags::POLLHUP) {
                if daemon.handle_pty_output() {
                    break;
                }
            }
        }

        // Client fds
        daemon.handle_client_data(&poll_fds[3..]);

        // If child exited and we've had a chance to drain PTY, exit
        if daemon.child_exited {
            let _ = daemon.handle_pty_output();
            break;
        }
    }
}

fn run_daemon(cfg: &Cfg, server_fd: RawFd) {
    // Ignore SIGPIPE early
    ignore_signal(Signal::SIGPIPE);

    // Spawn PTY first — the shell starts immediately and may send
    // DA queries within milliseconds. We need to be ready to respond.
    let shell = util::detect_shell();
    let (master_fd, child_pid) = match spawn_pty(&shell, 24, 80, &cfg.session_name) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: failed to spawn pty: {}", e);
            let _ = std::fs::remove_file(&cfg.socket_path);
            return;
        }
    };

    // Drain PTY output and respond to DA queries immediately.
    // Fish sends DA1 very early and has a short read timeout.
    let early_output = drain_da_queries(master_fd);

    // Now do remaining setup — logger, signals, etc.
    let log_system = Box::leak(Box::new(logger::LogSystem::new()));
    let log_path = cfg.socket_dir.join("logs").join(format!("{}.log", cfg.session_name));
    if let Err(e) = log_system.init(&log_path) {
        eprintln!("warning: failed to init log: {}", e);
    }
    let _ = log::set_logger(log_system);
    log::set_max_level(log::LevelFilter::Info);

    log::info!("daemon starting, session={}", cfg.session_name);

    // Setup signal pipe
    let (sig_read, _sig_write) = match create_signal_pipe() {
        Ok(fds) => fds,
        Err(e) => {
            log::error!("failed to create signal pipe: {}", e);
            return;
        }
    };

    // Install signal handlers
    install_signal_handler(Signal::SIGCHLD);
    install_signal_handler(Signal::SIGTERM);

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    log::info!("child spawned, pid={} shell={}", child_pid, shell);

    let mut daemon = Daemon {
        server_fd,
        pty_master_fd: master_fd,
        child_pid,
        clients: Vec::new(),
        parser: vt100::Parser::new(24, 80, 1000),
        session_name: cfg.session_name.clone(),
        shell_cmd: shell,
        cwd,
        created_at: now_epoch(),
        task_ended_at: 0,
        task_exit_code: 0,
        child_exited: false,
        has_had_client: false,
        signal_read_fd: sig_read,
    };

    // Feed any early PTY output into the parser
    if !early_output.is_empty() {
        daemon.parser.process(&early_output);
    }

    daemon_loop(&mut daemon);

    // Cleanup
    log::info!("daemon exiting, session={}", daemon.session_name);

    // Close all client connections
    for c in &daemon.clients {
        let _ = ipc::send(c.fd, Tag::Detach, &[]);
        unsafe { libc::close(c.fd); }
    }

    unsafe {
        libc::close(daemon.pty_master_fd);
        libc::close(daemon.server_fd);
    }

    // Remove socket file
    let _ = std::fs::remove_file(&cfg.socket_path);
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

fn run_client(socket_fd: RawFd) -> i32 {
    let stdin_fd: RawFd = 0;
    let stdout_fd: RawFd = 1;

    // Enter raw mode
    let saved = match enter_raw_mode(stdin_fd) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to enter raw mode: {}", e);
            return 1;
        }
    };
    let _guard = RawModeGuard { fd: stdin_fd, saved };

    // Setup SIGWINCH signal pipe
    ignore_signal(Signal::SIGPIPE);
    let (sig_read, _sig_write) = match create_signal_pipe() {
        Ok(fds) => fds,
        Err(e) => {
            eprintln!("error: failed to create signal pipe: {}", e);
            return 1;
        }
    };
    install_signal_handler(Signal::SIGWINCH);

    // Send initial resize
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

        // Signal — SIGWINCH
        if let Some(revents) = poll_fds[2].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 64];
                let _ = read_raw(signal_fd, &mut buf);
                let size = ipc::get_terminal_size(stdout_fd);
                let _ = ipc::send(socket_fd, Tag::Resize, size_as_bytes(&size));
            }
        }

        // stdin — user input
        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 4096];
                match read_raw(stdin_fd, &mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = &buf[..n];
                        // Check detach key: Ctrl+\ (0x1c) or Kitty protocol variant
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

        // Socket — data from daemon
        if let Some(revents) = poll_fds[1].revents() {
            if revents.contains(PollFlags::POLLIN) || revents.contains(PollFlags::POLLHUP) {
                match socket_buf.read(socket_fd) {
                    Ok(0) => {
                        // Daemon closed connection
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

// ---------------------------------------------------------------------------
// Attach flow
// ---------------------------------------------------------------------------

fn cmd_attach(name: &str, detached: bool) -> i32 {
    let current = socket::session_name_from_env();
    if !current.is_empty() {
        eprintln!("error: already inside session '{}'", current);
        return 1;
    }

    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    if let Err(e) = socket::ensure_dirs(&cfg.socket_dir) {
        eprintln!("error: failed to create directories: {}", e);
        return 1;
    }

    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s.to_string(),
        None => { eprintln!("error: invalid socket path"); return 1; }
    };

    // Check if session already exists
    match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            if detached {
                eprintln!("error: session '{}' already exists", name);
                return 1;
            }
            // Try to connect to existing session
            match socket::session_connect(&path_str) {
                Ok(fd) => {
                    return run_client(fd);
                }
                Err(_) => {
                    // Stale socket, clean up and create new
                    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                }
            }
        }
        Ok(false) => {} // No existing session, spawn new daemon
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    }

    // Spawn new daemon
    if detached {
        return match spawn_daemon_detached(&cfg) {
            Ok(()) => 0,
            Err(e) => { eprintln!("error: {}", e); 1 }
        };
    }

    let socket_fd = match spawn_daemon(&cfg) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    run_client(socket_fd)
}

fn spawn_daemon(cfg: &Cfg) -> Result<RawFd, String> {
    // Create server socket before fork — child inherits it,
    // parent closes it. Matches zmx's approach.
    let server_fd = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(server_fd); }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        // Child — becomes the daemon
        unsafe { libc::setsid(); }
        redirect_std_to_devnull();
        run_daemon(cfg, server_fd);
        unsafe { libc::_exit(0); }
    }

    // Parent — close inherited server fd, sleep briefly for shell
    // to start and send DA queries, then connect.
    unsafe { libc::close(server_fd); }
    std::thread::sleep(std::time::Duration::from_millis(10));

    let path_str = cfg.socket_path.to_str()
        .ok_or("invalid socket path")?;

    // Retry connect a few times in case daemon needs more time
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

fn spawn_daemon_detached(cfg: &Cfg) -> Result<(), String> {
    let server_fd = socket::create_socket(&cfg.socket_path)
        .map_err(|e| format!("failed to create socket: {}", e))?;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        unsafe { libc::close(server_fd); }
        let _ = std::fs::remove_file(&cfg.socket_path);
        return Err(format!("fork failed: {}", io::Error::last_os_error()));
    }

    if pid == 0 {
        unsafe { libc::setsid(); }
        redirect_std_to_devnull();
        run_daemon(cfg, server_fd);
        unsafe { libc::_exit(0); }
    }

    unsafe { libc::close(server_fd); }
    println!("session '{}' created", cfg.session_name);
    Ok(())
}

// ---------------------------------------------------------------------------
// History command
// ---------------------------------------------------------------------------

fn cmd_history(name: &str, format: util::HistoryFormat) -> i32 {
    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };
    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s,
        None => { eprintln!("error: invalid socket path"); return 1; }
    };
    let fd = match socket::session_connect(path_str) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("error: cannot connect to session '{}': {}", name, e); return 1; }
    };

    let format_byte = format as u8;
    if let Err(e) = ipc::send(fd, Tag::History, &[format_byte]) {
        eprintln!("error: failed to send history request: {}", e);
        unsafe { libc::close(fd); }
        return 1;
    }

    // Read response
    ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();
    let stdout_fd: RawFd = 1;

    loop {
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let mut poll_fds = [PollFd::new(sock_bfd, PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(5000u16)) {
            Ok(0) => { break; } // timeout
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        match socket_buf.read(fd) {
            Ok(0) => break,
            Ok(_) => {
                while let Some((tag, payload)) = socket_buf.next() {
                    if tag == Tag::History {
                        if !payload.is_empty() {
                            let _ = write_all_raw(stdout_fd, &payload);
                        }
                        unsafe { libc::close(fd); }
                        return 0;
                    }
                }
            }
            Err(_) => break,
        }
    }

    unsafe { libc::close(fd); }
    0
}

// ---------------------------------------------------------------------------
// Wait command
// ---------------------------------------------------------------------------

fn cmd_wait(names: &[String]) -> i32 {
    let socket_dir = socket::socket_dir();
    let prefix = socket::session_prefix();

    // If no names given, use prefix alone (wait for all prefixed sessions)
    let patterns: Vec<String> = if names.is_empty() {
        if prefix.is_empty() {
            eprintln!("error: wait requires session names or RYX_SESSION_PREFIX");
            return 1;
        }
        vec![prefix.clone()]
    } else {
        names.iter().map(|n| format!("{}{}", prefix, n)).collect()
    };

    let mut no_match_count = 0;
    let mut last_exit_code: i32 = 0;

    loop {
        let entries = match util::get_session_entries(&socket_dir) {
            Ok(e) => e,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::NotFound {
                    Vec::new()
                } else {
                    eprintln!("error: {}", e);
                    return 1;
                }
            }
        };

        // Find sessions matching any pattern (prefix match)
        let matching: Vec<&util::SessionEntry> = entries.iter()
            .filter(|e| patterns.iter().any(|p| e.name.starts_with(p)))
            .collect();

        if matching.is_empty() {
            no_match_count += 1;
            if no_match_count >= 3 {
                eprintln!("error: no matching sessions found");
                return 2;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        no_match_count = 0;

        let mut all_done = true;
        let mut any_unreachable = false;

        for session in &matching {
            if session.is_error {
                eprintln!("task unreachable: {}", session.name);
                any_unreachable = true;
                continue;
            }

            match session.task_ended_at {
                Some(t) if t > 0 => {
                    if let Some(code) = session.task_exit_code {
                        if code != 0 {
                            last_exit_code = code as i32;
                        }
                    }
                }
                _ => {
                    eprintln!("still waiting task={}", session.name);
                    all_done = false;
                }
            }
        }

        if any_unreachable {
            return 1;
        }

        if all_done {
            eprintln!("tasks completed!");
            return last_exit_code;
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// ---------------------------------------------------------------------------
// Run command
// ---------------------------------------------------------------------------

fn cmd_run(name: &str, cmd_args: &[String]) -> i32 {
    if cmd_args.is_empty() {
        eprintln!("error: run requires a command");
        return 1;
    }

    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    if let Err(e) = socket::ensure_dirs(&cfg.socket_dir) {
        eprintln!("error: failed to create directories: {}", e);
        return 1;
    }

    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s.to_string(),
        None => { eprintln!("error: invalid socket path"); return 1; }
    };

    // Ensure session exists
    let socket_fd = match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            match socket::session_connect(&path_str) {
                Ok(fd) => fd,
                Err(_) => {
                    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                    match spawn_daemon(&cfg) {
                        Ok(fd) => fd,
                        Err(e) => { eprintln!("error: {}", e); return 1; }
                    }
                }
            }
        }
        Ok(false) => {
            match spawn_daemon(&cfg) {
                Ok(fd) => fd,
                Err(e) => { eprintln!("error: {}", e); return 1; }
            }
        }
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    // Build shell-quoted command string
    let cmd_str: String = cmd_args.iter()
        .map(|a| {
            if util::shell_needs_quoting(a) {
                util::shell_quote(a)
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    // Send Run command
    if let Err(e) = ipc::send(socket_fd, Tag::Run, cmd_str.as_bytes()) {
        eprintln!("error: failed to send command: {}", e);
        unsafe { libc::close(socket_fd); }
        return 1;
    }

    // Read output until Ack
    ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();
    let stdout_fd: RawFd = 1;

    loop {
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(socket_fd) };
        let mut poll_fds = [PollFd::new(sock_bfd, PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        match socket_buf.read(socket_fd) {
            Ok(0) => break,
            Ok(_) => {
                while let Some((tag, payload)) = socket_buf.next() {
                    match tag {
                        Tag::Output => {
                            let _ = write_all_raw(stdout_fd, &payload);
                        }
                        Tag::Ack => {
                            let code = if payload.is_empty() { 0 } else { payload[0] as i32 };
                            unsafe { libc::close(socket_fd); }
                            return code;
                        }
                        _ => {}
                    }
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(_) => break,
        }
    }

    unsafe { libc::close(socket_fd); }
    1
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
