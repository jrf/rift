use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::Signal;

use crate::daemon::{self, Cfg};
use crate::ipc::{self, Tag, SocketBuffer};
use crate::socket;
use crate::util;

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn cmd_list(short: bool) -> i32 {
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };
    let path_str = match socket_path.to_str() {
        Some(s) => s,
        None => { eprintln!("error: invalid socket path"); return 1; }
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
            unsafe { libc::kill(pid, libc::SIGKILL); }
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
        unsafe { libc::kill(pid, libc::SIGTERM); }
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let mut code = 0;
    for name in &session_names {
        let r = kill_one(&socket_dir, name, force);
        if r != 0 { code = r; }
    }
    code
}

// ---------------------------------------------------------------------------
// detach
// ---------------------------------------------------------------------------

pub fn cmd_detach(name: &str) -> i32 {
    let fd = match util::session_connect_by_name(name) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
            eprintln!("error: wait requires session names or RIF_SESSION_PREFIX");
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

        let matching: Vec<&util::SessionEntry> = entries.iter()
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

    let socket_fd: OwnedFd = match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            match socket::session_connect(&path_str) {
                Ok(fd) => fd,
                Err(_) => {
                    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);
                    match daemon::spawn_daemon(&cfg, &[]) {
                        Ok(fd) => fd,
                        Err(e) => { eprintln!("error: {}", e); return 1; }
                    }
                }
            }
        }
        Ok(false) => {
            match daemon::spawn_daemon(&cfg, &[]) {
                Ok(fd) => fd,
                Err(e) => { eprintln!("error: {}", e); return 1; }
            }
        }
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

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

    let wrapped = if fish {
        format!("{}; printf 'RIF_TASK_COMPLETED:%d' $status\n", cmd_str)
    } else {
        format!("{}; printf 'RIF_TASK_COMPLETED:%d' $?\n", cmd_str)
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
                        Tag::Ack => {
                            return if payload.is_empty() { 0 } else { payload[0] as i32 };
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
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
            format!("printf '{}' | base64 -d > {}\n", encoded, util::shell_quote(path))
        } else {
            format!("printf '{}' | base64 -d >> {}\n", encoded, util::shell_quote(path))
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
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    let mut fds: Vec<OwnedFd> = Vec::new();
    let mut bufs: Vec<SocketBuffer> = Vec::new();

    for name in &session_names {
        let socket_path = match socket::get_socket_path(&socket_dir, name) {
            Ok(p) => p,
            Err(e) => { eprintln!("error: {}", e); continue; }
        };
        let path_str = match socket_path.to_str() {
            Some(s) => s,
            None => { eprintln!("error: invalid socket path"); continue; }
        };
        match socket::session_connect(path_str) {
            Ok(fd) => {
                fds.push(fd);
                bufs.push(SocketBuffer::new());
            }
            Err(e) => { eprintln!("error: cannot connect to session '{}': {}", name, e); }
        }
    }

    if fds.is_empty() {
        return 1;
    }

    daemon::ignore_signal(Signal::SIGPIPE);
    let stdout_fd: RawFd = 1;

    loop {
        let mut poll_fds: Vec<PollFd> = fds.iter()
            .map(|fd| {
                let bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
                PollFd::new(bfd, PollFlags::POLLIN)
            })
            .collect();

        match poll(&mut poll_fds, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        let mut closed = Vec::new();
        for i in 0..fds.len() {
            let revents = match poll_fds[i].revents() {
                Some(r) => r,
                None => continue,
            };
            if !revents.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
                continue;
            }
            match bufs[i].read(fds[i].as_raw_fd()) {
                Ok(0) => { closed.push(i); }
                Ok(_) => {
                    while let Some((tag, payload)) = bufs[i].next() {
                        if tag == Tag::Output {
                            let _ = ipc::write_all(stdout_fd, payload);
                        }
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => {}
                Err(_) => { closed.push(i); }
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

    match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            if detached {
                eprintln!("error: session '{}' already exists", name);
                return 1;
            }
            match socket::session_connect(&path_str) {
                Ok(fd) => {
                    return daemon::run_client(fd);
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
            Err(e) => { eprintln!("error: {}", e); 1 }
        };
    }

    let socket_fd = match daemon::spawn_daemon(&cfg, cmd) {
        Ok(fd) => fd,
        Err(e) => { eprintln!("error: {}", e); return 1; }
    };

    daemon::run_client(socket_fd)
}
