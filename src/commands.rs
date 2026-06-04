use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::sys::signal::Signal;

use crate::daemon::{self, Cfg};
use crate::ipc::{self, SocketBuffer, Tag};
use crate::socket;
use crate::util;

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn cmd_list(short: bool, verbose: bool) -> i32 {
    let socket_dir = socket::socket_dir();
    let current = socket::session_name_from_env();
    let current_ref = if current.is_empty() {
        None
    } else {
        Some(current.as_str())
    };

    match util::get_session_entries(&socket_dir) {
        Ok(entries) => {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for entry in &entries {
                let _ = util::write_session_line(
                    &mut out,
                    entry,
                    short,
                    verbose,
                    &socket_dir,
                    current_ref,
                );
            }
            0
        }
        Err(e) => {
            if e.kind() == io::ErrorKind::NotFound {
                0
            } else {
                eprintln!("error: {}", e);
                1
            }
        }
    }
}

// ---------------------------------------------------------------------------
// kill
// ---------------------------------------------------------------------------

fn kill_one(socket_dir: &Path, session_name: &str, force: bool) -> i32 {
    let socket_path = match socket::get_socket_path(socket_dir, session_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let path_str = match socket_path.to_str() {
        Some(s) => s,
        None => {
            eprintln!("error: invalid socket path");
            return 1;
        }
    };

    let pid = match ipc::probe_session(path_str) {
        Ok(result) => {
            let pid = result.info.pid;
            if !force {
                let _ = ipc::send(result.fd.as_raw_fd(), Tag::Kill, &[]);
            }
            Some(pid)
        }
        Err(_) => {
            if !force {
                match socket::session_connect(path_str) {
                    Ok(fd) => {
                        let _ = ipc::send(fd.as_raw_fd(), Tag::Kill, &[]);
                    }
                    Err(e) => {
                        if e.kind() == io::ErrorKind::ConnectionRefused {
                            socket::cleanup_stale_socket(socket_dir, session_name);
                            return 0;
                        }
                        eprintln!("error: cannot connect to session '{}': {}", session_name, e);
                        return 1;
                    }
                }
            }
            None
        }
    };

    if force {
        if let Some(pid) = pid {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        socket::cleanup_stale_socket(socket_dir, session_name);
        return 0;
    }

    for _ in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(200));
        if let Ok(false) = socket::session_exists(socket_dir, session_name) {
            return 0;
        }
    }

    if let Some(pid) = pid {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(200));
            if let Ok(false) = socket::session_exists(socket_dir, session_name) {
                return 0;
            }
        }
    }

    socket::cleanup_stale_socket(socket_dir, session_name);
    0
}

pub fn cmd_kill(names: &[String], force: bool) -> i32 {
    let socket_dir = socket::socket_dir();

    let session_names = match util::resolve_sessions(&socket_dir, names) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let mut code = 0;
    for name in &session_names {
        let r = kill_one(&socket_dir, name, force);
        if r != 0 {
            code = r;
        }
    }
    code
}

// ---------------------------------------------------------------------------
// detach
// ---------------------------------------------------------------------------

pub fn cmd_detach(name: &str) -> i32 {
    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::DetachAll, &[]) {
        eprintln!("error: failed to send detach: {}", e);
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

pub fn cmd_history(name: &str, format: util::HistoryFormat) -> i32 {
    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let format_byte = format as u8;
    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::History, &[format_byte]) {
        eprintln!("error: failed to send history request: {}", e);
        return 1;
    }

    daemon::ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();
    let stdout_fd: RawFd = 1;

    loop {
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(sock_bfd, PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(5000u16)) {
            Ok(0) => break,
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        match socket_buf.read(fd.as_raw_fd()) {
            Ok(0) => break,
            Ok(_) => {
                while let Some((tag, payload)) = socket_buf.next() {
                    if tag == Tag::History {
                        if !payload.is_empty() {
                            let _ = ipc::write_all(stdout_fd, payload);
                        }
                        return 0;
                    }
                }
            }
            Err(_) => break,
        }
    }

    0
}

// ---------------------------------------------------------------------------
// wait
// ---------------------------------------------------------------------------

pub fn cmd_wait(names: &[String]) -> i32 {
    let socket_dir = socket::socket_dir();
    let prefix = socket::session_prefix();

    let patterns: Vec<String> = if names.is_empty() {
        if prefix.is_empty() {
            eprintln!("error: wait requires session names or RIFT_SESSION_PREFIX");
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
                if e.kind() == io::ErrorKind::NotFound {
                    Vec::new()
                } else {
                    eprintln!("error: {}", e);
                    return 1;
                }
            }
        };

        let matching: Vec<&util::SessionEntry> = entries
            .iter()
            .filter(|e| util::pattern_matches(&patterns, &e.name))
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
// run
// ---------------------------------------------------------------------------

pub fn cmd_run(name: &str, cmd_args: &[String], detached: bool, fish: bool) -> i32 {
    if cmd_args.is_empty() {
        eprintln!("error: run requires a command");
        return 1;
    }

    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    if let Err(e) = socket::ensure_dirs(&cfg.socket_dir) {
        eprintln!("error: failed to create directories: {}", e);
        return 1;
    }

    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s.to_string(),
        None => {
            eprintln!("error: invalid socket path");
            return 1;
        }
    };

    let socket_fd: OwnedFd = match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => match socket::session_connect(&path_str) {
            Ok(fd) => fd,
            Err(_) => {
                socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                match daemon::spawn_daemon(&cfg, &[]) {
                    Ok(fd) => fd,
                    Err(e) => {
                        eprintln!("error: {}", e);
                        return 1;
                    }
                }
            }
        },
        Ok(false) => match daemon::spawn_daemon(&cfg, &[]) {
            Ok(fd) => fd,
            Err(e) => {
                eprintln!("error: {}", e);
                return 1;
            }
        },
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let cmd_str: String = cmd_args
        .iter()
        .map(|a| {
            if util::shell_needs_quoting(a) {
                util::shell_quote(a)
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let wrapped = if fish {
        format!("{}; printf 'RIFT_TASK_COMPLETED:%d' $status\n", cmd_str)
    } else {
        format!("{}; printf 'RIFT_TASK_COMPLETED:%d' $?\n", cmd_str)
    };

    if let Err(e) = ipc::send(socket_fd.as_raw_fd(), Tag::Run, wrapped.as_bytes()) {
        eprintln!("error: failed to send command: {}", e);
        return 1;
    }

    if detached {
        return 0;
    }

    daemon::ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();
    let stdout_fd: RawFd = 1;

    loop {
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(socket_fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(sock_bfd, PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        match socket_buf.read(socket_fd.as_raw_fd()) {
            Ok(0) => break,
            Ok(_) => {
                while let Some((tag, payload)) = socket_buf.next() {
                    match tag {
                        Tag::Output => {
                            let _ = ipc::write_all(stdout_fd, payload);
                        }
                        _ => {}
                    }
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(_) => break,
        }
    }

    1
}

// ---------------------------------------------------------------------------
// send
// ---------------------------------------------------------------------------

pub fn cmd_send(name: &str, text_args: &[String]) -> i32 {
    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let data = if text_args.is_empty() {
        let mut buf = Vec::new();
        if io::Read::read_to_end(&mut io::stdin(), &mut buf).is_err() {
            eprintln!("error: failed to read stdin");
            return 1;
        }
        buf
    } else {
        text_args.join(" ").into_bytes()
    };

    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::Input, &data) {
        eprintln!("error: failed to send: {}", e);
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// print
// ---------------------------------------------------------------------------

pub fn cmd_print(name: &str, text_args: &[String]) -> i32 {
    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let data = if text_args.is_empty() {
        let mut buf = Vec::new();
        if io::Read::read_to_end(&mut io::stdin(), &mut buf).is_err() {
            eprintln!("error: failed to read stdin");
            return 1;
        }
        buf
    } else {
        let mut s = text_args.join(" ");
        s.push('\n');
        s.into_bytes()
    };

    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::Print, &data) {
        eprintln!("error: failed to send: {}", e);
        return 1;
    }
    0
}

// ---------------------------------------------------------------------------
// write
// ---------------------------------------------------------------------------

pub fn cmd_write(name: &str, path: &str) -> i32 {
    use std::io::Read;

    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let mut stdin_data = Vec::new();
    if io::stdin().read_to_end(&mut stdin_data).is_err() {
        eprintln!("error: failed to read stdin");
        return 1;
    }

    if stdin_data.is_empty() {
        eprintln!("error: no data on stdin");
        return 1;
    }

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    const CHUNK_SIZE: usize = 48 * 1024;
    let chunks: Vec<&[u8]> = stdin_data.chunks(CHUNK_SIZE).collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let encoded = engine.encode(chunk);
        let cmd = if i == 0 {
            format!(
                "printf '{}' | base64 -d > {}\n",
                encoded,
                util::shell_quote(path)
            )
        } else {
            format!(
                "printf '{}' | base64 -d >> {}\n",
                encoded,
                util::shell_quote(path)
            )
        };
        if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::Input, cmd.as_bytes()) {
            eprintln!("error: failed to send chunk: {}", e);
            return 1;
        }
        if i < chunks.len() - 1 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    0
}

// ---------------------------------------------------------------------------
// tail
// ---------------------------------------------------------------------------

pub fn cmd_tail(names: &[String]) -> i32 {
    let socket_dir = socket::socket_dir();

    let session_names = match util::resolve_sessions(&socket_dir, names) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut bufs: Vec<SocketBuffer> = Vec::new();

    for name in &session_names {
        let socket_path = match socket::get_socket_path(&socket_dir, name) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {}", e);
                continue;
            }
        };
        let path_str = match socket_path.to_str() {
            Some(s) => s,
            None => {
                eprintln!("error: invalid socket path");
                continue;
            }
        };
        match socket::session_connect(path_str) {
            Ok(fd) => {
                fds.push(fd);
                bufs.push(SocketBuffer::new());
            }
            Err(e) => {
                eprintln!("error: cannot connect to session '{}': {}", name, e);
            }
        }
    }

    if fds.is_empty() {
        return 1;
    }

    daemon::ignore_signal(Signal::SIGPIPE);
    let stdout_fd: RawFd = 1;

    loop {
        let stdin_bfd = unsafe { BorrowedFd::borrow_raw(0) };
        let mut poll_fds: Vec<PollFd> = vec![PollFd::new(stdin_bfd, PollFlags::POLLIN)];
        for fd in &fds {
            let bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
            poll_fds.push(PollFd::new(bfd, PollFlags::POLLIN));
        }

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        if let Some(revents) = poll_fds[0].revents() {
            if revents.contains(PollFlags::POLLIN) {
                let mut buf = [0u8; 128];
                let stdin_bfd = unsafe { BorrowedFd::borrow_raw(0) };
                if let Ok(n) = nix::unistd::read(&stdin_bfd, &mut buf) {
                    if n > 0 && buf[..n].contains(&0x03) {
                        return 0;
                    }
                }
            }
        }

        let mut closed = Vec::new();
        for i in 0..fds.len() {
            let revents = match poll_fds[i + 1].revents() {
                Some(r) => r,
                None => continue,
            };
            if !revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
                continue;
            }
            match bufs[i].read(fds[i].as_raw_fd()) {
                Ok(0) => {
                    closed.push(i);
                }
                Ok(_) => {
                    while let Some((tag, payload)) = bufs[i].next() {
                        if tag == Tag::Output {
                            let filtered = util::filter_tail_output(payload);
                            let _ = ipc::write_all(stdout_fd, &filtered);
                        }
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => {}
                Err(_) => {
                    closed.push(i);
                }
            }
        }

        for &i in closed.iter().rev() {
            fds.remove(i);
            bufs.remove(i);
        }

        if fds.is_empty() {
            break;
        }
    }

    0
}

// ---------------------------------------------------------------------------
// logs
// ---------------------------------------------------------------------------

pub fn cmd_logs(name: &str, extra_args: &[String]) -> i32 {
    let prefix = socket::session_prefix();
    let session_name = match socket::get_session_name(&prefix, name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let log_path = socket::socket_dir()
        .join("logs")
        .join(format!("{}.log", session_name));
    if !log_path.exists() {
        eprintln!(
            "error: no log file for session '{}' at {}",
            name,
            log_path.display()
        );
        return 1;
    }
    let mut tail_args: Vec<String> = if extra_args.is_empty() {
        vec!["-f".to_string()]
    } else {
        extra_args.to_vec()
    };
    tail_args.push(log_path.to_string_lossy().into_owned());
    match std::process::Command::new("tail").args(&tail_args).status() {
        Ok(status) => status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("error: failed to exec tail: {}", e);
            1
        }
    }
}

// ---------------------------------------------------------------------------
// attach
// ---------------------------------------------------------------------------

pub fn cmd_attach(name: &str, detached: bool, cmd: &[String]) -> i32 {
    let current = socket::session_name_from_env();
    if !current.is_empty() {
        eprintln!("error: already inside session '{}'", current);
        return 1;
    }

    let cfg = match Cfg::resolve(name) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    if let Err(e) = socket::ensure_dirs(&cfg.socket_dir) {
        eprintln!("error: failed to create directories: {}", e);
        return 1;
    }

    let path_str = match cfg.socket_path.to_str() {
        Some(s) => s.to_string(),
        None => {
            eprintln!("error: invalid socket path");
            return 1;
        }
    };

    match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            if detached {
                eprintln!("error: session '{}' already exists", name);
                return 1;
            }
            match socket::session_connect(&path_str) {
                Ok(fd) => {
                    util::write_last_session(&cfg.socket_dir, name);
                    util::run_hook("RIFT_ON_ATTACH", &cfg.session_name);
                    let code = daemon::run_client(fd);
                    util::run_hook("RIFT_ON_DETACH", &cfg.session_name);
                    return code;
                }
                Err(_) => {
                    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                }
            }
        }
        Ok(false) => {}
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    }

    if detached {
        return match daemon::spawn_daemon_detached(&cfg, cmd) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {}", e);
                1
            }
        };
    }

    let socket_fd = match daemon::spawn_daemon(&cfg, cmd) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    util::write_last_session(&cfg.socket_dir, name);
    util::run_hook("RIFT_ON_ATTACH", &cfg.session_name);
    let code = daemon::run_client(socket_fd);
    util::run_hook("RIFT_ON_DETACH", &cfg.session_name);
    code
}

// ---------------------------------------------------------------------------
// pick (interactive picker — invoked by bare `rift` with no args)
// ---------------------------------------------------------------------------

pub fn cmd_pick() -> i32 {
    let socket_dir = socket::socket_dir();
    let entries = match util::get_session_entries(&socket_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if entries.is_empty() {
        eprintln!("no sessions — `rift help` or `rift <name>` to create one");
        return 0;
    }
    let names: Vec<String> = entries.into_iter().map(|e| e.name).collect();

    let picker_env = std::env::var("RIFT_PICKER").ok().filter(|s| !s.is_empty());
    let picked = match picker_env {
        Some(cmd) => run_external_picker(&cmd, &names),
        None => run_builtin_picker(&names),
    };
    let full_name = match picked {
        Some(n) => n,
        None => return 0,
    };

    let prefix = socket::session_prefix();
    let bare = full_name
        .strip_prefix(&prefix)
        .unwrap_or(&full_name)
        .to_string();
    cmd_attach(&bare, false, &[])
}

fn run_external_picker(picker_cmd: &str, names: &[String]) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(picker_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to spawn picker: {}", e);
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(names.join("\n").as_bytes());
    }
    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(_) => return None,
    };
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout);
    s.lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

fn run_builtin_picker(names: &[String]) -> Option<String> {
    use std::io::{BufRead, BufReader, Write};

    let mut tty = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    {
        Ok(f) => f,
        Err(_) => {
            eprintln!("error: no controlling tty");
            return None;
        }
    };
    for (i, name) in names.iter().enumerate() {
        let _ = writeln!(tty, "{:3}  {}", i + 1, name);
    }
    let _ = write!(tty, "> ");
    let _ = tty.flush();

    let mut reader = BufReader::new(tty);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return None;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(n) = trimmed.parse::<usize>() {
        if (1..=names.len()).contains(&n) {
            return Some(names[n - 1].clone());
        }
        eprintln!("error: out of range");
        return None;
    }
    if names.iter().any(|n| n == trimmed) {
        return Some(trimmed.to_string());
    }
    eprintln!("error: no session matches '{}'", trimmed);
    None
}

// ---------------------------------------------------------------------------
// last
// ---------------------------------------------------------------------------

pub fn cmd_last() -> i32 {
    let socket_dir = socket::socket_dir();
    let name = match util::read_last_session(&socket_dir) {
        Some(n) => n,
        None => {
            eprintln!("error: no recent session");
            return 1;
        }
    };
    cmd_attach(&name, false, &[])
}

// ---------------------------------------------------------------------------
// rename
// ---------------------------------------------------------------------------

pub fn cmd_rename(name: &str, new_name: &str) -> i32 {
    if name == new_name {
        return 0;
    }

    let prefix = socket::session_prefix();
    let new_session_name = match socket::get_session_name(&prefix, new_name) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    let socket_dir = socket::socket_dir();
    let _ = match socket::get_socket_path(&socket_dir, &new_session_name) {
        Ok(p) => p,
        Err(_) => {
            socket::print_session_name_too_long(&new_session_name, &socket_dir);
            return 1;
        }
    };

    if socket::session_exists(&socket_dir, &new_session_name).unwrap_or(false) {
        eprintln!("error: session '{}' already exists", new_name);
        return 1;
    }

    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::Rename, new_session_name.as_bytes()) {
        eprintln!("error: failed to send rename request: {}", e);
        return 1;
    }

    0
}
