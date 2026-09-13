use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::sys::signal::Signal;

use crate::daemon::{self, Cfg};
use crate::ipc::{self, SocketBuffer, Tag};
use crate::label;
use crate::socket;
use crate::util;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> u64 {
    let counter = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    (pid << 32) | counter
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

pub fn cmd_list(short: bool, verbose: bool, where_pair: Option<&str>) -> i32 {
    let filter = match where_pair.map(label::parse_pair).transpose() {
        Ok(filter) => filter,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
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
            for entry in entries.iter().filter(|entry| {
                let Some((key, expected)) = filter else {
                    return true;
                };
                entry
                    .labels
                    .as_deref()
                    .map(label::decode)
                    .and_then(|labels| labels.get(key).cloned())
                    .is_some_and(|value| value == expected)
            }) {
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
// labels
// ---------------------------------------------------------------------------

fn session_request(
    name: &str,
    what: &str,
    request_tag: Tag,
    payload: &[u8],
    response_tag: Tag,
) -> Result<Vec<u8>, String> {
    let cfg = Cfg::resolve(name)?;
    let socket_path = cfg.socket_path.to_str().ok_or("invalid socket path")?;
    ipc::request_response(socket_path, request_tag, payload, response_tag).map_err(|error| {
        if matches!(error, ipc::ProbeError::Timeout) {
            format!(
                "session '{}' did not respond to the {} request; restart its daemon with this Rift version",
                cfg.session_name, what
            )
        } else {
            format!("{} request for '{}': {}", what, cfg.session_name, error)
        }
    })
}

fn label_request(
    name: &str,
    request_tag: Tag,
    payload: &[u8],
    response_tag: Tag,
) -> Result<Vec<u8>, String> {
    session_request(name, "label", request_tag, payload, response_tag)
}

pub fn cmd_label_get(name: &str, key: Option<&str>) -> i32 {
    if let Some(key) = key
        && let Err(error) = label::validate_key(key)
    {
        eprintln!("error: {error}");
        return 1;
    }
    let payload = match label_request(name, Tag::LabelGet, &[], Tag::LabelData) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let labels = String::from_utf8_lossy(&payload);
    if let Some(key) = key {
        match label::decode(&labels).get(key) {
            Some(value) => print!("{value}"),
            None => {
                eprintln!("error: label key not found: {key}");
                return 1;
            }
        }
    } else {
        print!("{labels}");
    }
    0
}

pub fn cmd_label_set(name: &str, pairs: &[String]) -> i32 {
    if pairs.is_empty() {
        eprintln!("error: set requires at least one key=value label");
        return 1;
    }
    let expanded: Vec<&str> = pairs
        .iter()
        .flat_map(|group| group.split_whitespace())
        .collect();
    if expanded.is_empty() {
        eprintln!("error: set requires at least one key=value label");
        return 1;
    }
    for pair in &expanded {
        if let Err(error) = label::parse_pair(pair) {
            eprintln!("error: {error}");
            return 1;
        }
    }
    let payload = expanded.join(" ");
    match label_request(name, Tag::LabelSet, payload.as_bytes(), Tag::Ack) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

pub fn cmd_label_unset(name: &str, keys: &[String]) -> i32 {
    if keys.is_empty() {
        eprintln!("error: unset requires at least one label key");
        return 1;
    }
    let mut pairs = Vec::with_capacity(keys.len());
    for key in keys {
        if let Err(error) = label::validate_key(key) {
            eprintln!("error: {error}");
            return 1;
        }
        pairs.push(format!("{key}="));
    }
    match label_request(name, Tag::LabelSet, pairs.join(" ").as_bytes(), Tag::Ack) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

pub fn cmd_label_clear(name: &str) -> i32 {
    match label_request(name, Tag::LabelClear, &[], Tag::Ack) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// print-env
// ---------------------------------------------------------------------------

pub fn cmd_print_env(name: &str, key: Option<&str>, shell: bool) -> i32 {
    let payload = match session_request(name, "print-env", Tag::EnvGet, &[], Tag::EnvData) {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    let text = String::from_utf8_lossy(&payload);

    if let Some(key) = key {
        match crate::env::value_of(&text, key) {
            Some(value) => {
                println!("{value}");
                0
            }
            None => {
                eprintln!("error: environment variable not found: {key}");
                1
            }
        }
    } else if shell {
        print!("{}", crate::env::to_shell(&text));
        0
    } else {
        print!("{text}");
        0
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

pub fn cmd_history(name: &str, format: ipc::HistoryFormat) -> i32 {
    let data = match fetch_history(name, format) {
        Ok(data) => data,
        Err(error) => {
            eprintln!("error: {}", error);
            return 1;
        }
    };
    if !data.is_empty() {
        let _ = ipc::write_all(1, &data);
    }
    0
}

fn fetch_history(name: &str, format: ipc::HistoryFormat) -> Result<Vec<u8>, String> {
    let fd = util::session_connect_by_name(name).map_err(|error| error.to_string())?;

    ipc::send(fd.as_raw_fd(), Tag::History, &format.encode())
        .map_err(|error| format!("failed to send history request: {}", error))?;

    daemon::ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();

    loop {
        let sock_bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(sock_bfd, PollFlags::POLLIN)];

        match poll(&mut poll_fds, PollTimeout::from(5000u16)) {
            Ok(0) => return Err("timed out waiting for history".to_string()),
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(format!("failed waiting for history: {}", error)),
        }

        match socket_buf.read(fd.as_raw_fd()) {
            Ok(0) => return Err("session closed before returning history".to_string()),
            Ok(_) => {
                while let Some((tag, payload)) = socket_buf
                    .next()
                    .map_err(|error| format!("invalid history response: {error}"))?
                {
                    if tag == Tag::History {
                        return Ok(payload.to_vec());
                    }
                }
            }
            Err(nix::errno::Errno::EAGAIN) => {}
            Err(error) => return Err(format!("failed reading history: {}", error)),
        }
    }
}

fn report_failed_task(session: &util::SessionEntry) {
    let exit_code = session.task_exit_code.unwrap_or(1);
    eprintln!("failed task={} exit_status={}", session.name, exit_code);

    match fetch_history(&session.name, ipc::HistoryFormat::Plain) {
        Ok(history) => {
            let text = String::from_utf8_lossy(&history);
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(20);
            eprintln!(
                "last {} lines of {} history:",
                lines.len() - start,
                session.name
            );
            for line in &lines[start..] {
                eprintln!("{}", line);
            }
        }
        Err(error) => eprintln!("history unavailable for {}: {}", session.name, error),
    }
    eprintln!("inspect with: rift history {}", session.name);
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
    let mut max_seen = 0;
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

        if matching.len() < max_seen {
            eprintln!(
                "error: {} session(s) disappeared before completing",
                max_seen - matching.len()
            );
            return 1;
        }

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
        max_seen = max_seen.max(matching.len());

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
                    if let Some(code) = session.task_exit_code
                        && code != 0
                    {
                        last_exit_code = code as i32;
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
            if last_exit_code == 0 {
                eprintln!("tasks completed!");
            } else {
                eprintln!("tasks failed!");
                for session in matching {
                    if session.task_exit_code.unwrap_or(0) != 0 {
                        report_failed_task(session);
                    }
                }
            }
            return last_exit_code;
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

// ---------------------------------------------------------------------------
// run
// ---------------------------------------------------------------------------

fn run_command(cmd_args: &[String]) -> Result<String, String> {
    if !cmd_args.is_empty() {
        return Ok(cmd_args
            .iter()
            .map(|arg| {
                if util::shell_needs_quoting(arg) {
                    util::shell_quote(arg)
                } else {
                    arg.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" "));
    }

    let stdin = io::stdin();
    if stdin.is_terminal() {
        return Err("run requires a command or piped stdin".to_string());
    }

    let mut command = String::new();
    stdin
        .lock()
        .read_to_string(&mut command)
        .map_err(|error| format!("failed to read stdin: {}", error))?;
    if command.is_empty() {
        Err("run requires a command or non-empty stdin".to_string())
    } else {
        Ok(command)
    }
}

pub fn cmd_run(name: &str, cmd_args: &[String], detached: bool, fish: bool) -> i32 {
    let cmd_str = match run_command(cmd_args) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("error: {}", error);
            return 1;
        }
    };

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
                match daemon::spawn_daemon(&cfg, &[], &[]) {
                    Ok(fd) => fd,
                    Err(e) => {
                        eprintln!("error: {}", e);
                        return 1;
                    }
                }
            }
        },
        Ok(false) => match daemon::spawn_daemon(&cfg, &[], &[]) {
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

    let request_id = next_request_id();
    let wrapped = if fish {
        format!(
            "{}\nset __rift_status $status; printf '\\nRIFT_TASK_REQUEST_COMPLETED:{}:%d\\n' $__rift_status; printf 'RIFT_TASK_%s:%d\\n' COMPLETED $__rift_status\n",
            cmd_str, request_id
        )
    } else {
        format!(
            "{}\n__rift_status=$?; printf '\\nRIFT_TASK_REQUEST_COMPLETED:{}:%d\\n' \"$__rift_status\"; printf 'RIFT_TASK_%s:%d\\n' COMPLETED \"$__rift_status\"\n",
            cmd_str, request_id
        )
    };

    // On the detached path there is no interactive terminal to mirror, so
    // avoid probing (which falls back to opening /dev/tty and can pick up the
    // launching terminal's size). Use a stable default instead.
    let size = if detached {
        ipc::Resize {
            rows: 24,
            cols: 120,
            xpixel: 0,
            ypixel: 0,
        }
    } else {
        ipc::get_terminal_size(libc::STDOUT_FILENO)
    };
    if let Err(e) = ipc::send(socket_fd.as_raw_fd(), Tag::Resize, &size.encode()) {
        eprintln!("error: failed to send terminal size: {}", e);
        return 1;
    }

    if let Err(e) = ipc::send(socket_fd.as_raw_fd(), Tag::Run, wrapped.as_bytes()) {
        eprintln!("error: failed to send command: {}", e);
        return 1;
    }

    if detached {
        return 0;
    }

    daemon::ignore_signal(Signal::SIGPIPE);
    let mut socket_buf = SocketBuffer::new();
    let mut task_scan_carry = Vec::new();
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
            Ok(_) => loop {
                let frame = match socket_buf.next() {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("error: invalid session response: {error}");
                        return 1;
                    }
                };
                let (tag, payload) = frame;
                match tag {
                    Tag::Output => {
                        let completions =
                            util::scan_task_completions(&mut task_scan_carry, payload);
                        let responses = util::device_attribute_responses(payload);
                        if !responses.is_empty() {
                            let _ = ipc::send(socket_fd.as_raw_fd(), Tag::Input, &responses);
                        }
                        let _ = ipc::write_all(stdout_fd, payload);
                        if let Some((_, exit_code)) = completions
                            .into_iter()
                            .find(|(completed_id, _)| *completed_id == request_id)
                        {
                            return exit_code as i32;
                        }
                    }
                    Tag::TaskComplete => {
                        if let Some((completed_id, exit_code)) = ipc::decode_task_complete(payload)
                            && completed_id == request_id
                        {
                            return exit_code as i32;
                        }
                    }
                    _ => {}
                }
            },
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
        text_args.join(" ").into_bytes()
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

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    const CHUNK_SIZE: usize = 48 * 1024;
    let chunks: Vec<&[u8]> = if stdin_data.is_empty() {
        vec![&[]]
    } else {
        stdin_data.chunks(CHUNK_SIZE).collect()
    };

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
                if let Err(error) = ipc::send(fd.as_raw_fd(), Tag::Tail, &[]) {
                    eprintln!("error: cannot subscribe to session '{}': {}", name, error);
                    continue;
                }
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

        if let Some(revents) = poll_fds[0].revents()
            && revents.contains(PollFlags::POLLIN)
        {
            let mut buf = [0u8; 128];
            let stdin_bfd = unsafe { BorrowedFd::borrow_raw(0) };
            if let Ok(n) = nix::unistd::read(stdin_bfd, &mut buf)
                && n > 0
                && buf[..n].contains(&0x03)
            {
                return 0;
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
                Ok(_) => loop {
                    let frame = match bufs[i].next() {
                        Ok(Some(frame)) => frame,
                        Ok(None) => break,
                        Err(error) => {
                            eprintln!("error: invalid session response: {error}");
                            closed.push(i);
                            break;
                        }
                    };
                    let (tag, payload) = frame;
                    if tag == Tag::Output {
                        let filtered = util::filter_tail_output(payload);
                        let _ = ipc::write_all(stdout_fd, &filtered);
                    }
                },
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

pub fn cmd_attach(name: &str, detached: bool, cmd: &[String], labels: &[String]) -> i32 {
    // Validate labels up front so an invalid pair fails before we create or
    // switch anything. Each --labels value may hold several space-separated
    // pairs (e.g. "project=pico env=prod"), matching `rift set`.
    for group in labels {
        for pair in group.split_whitespace() {
            if let Err(error) = label::parse_pair(pair) {
                eprintln!("error: {error}");
                return 1;
            }
        }
    }
    cmd_attach_with_policy(name, detached, cmd, true, labels)
}

fn cmd_attach_new(name: &str, cmd: &[String]) -> i32 {
    cmd_attach_with_policy(name, false, cmd, false, &[])
}

fn cmd_attach_with_policy(
    name: &str,
    detached: bool,
    cmd: &[String],
    allow_existing: bool,
    labels: &[String],
) -> i32 {
    let current = socket::session_name_from_env();
    if !current.is_empty() {
        // We're running *inside* a rift session. Rather than refuse, ask the
        // current session's daemon to switch the interactive user over to the
        // requested session (tmux/zmx-style in-session switching). A detached
        // spawn (`--detached`) still just creates the session in the background.
        if detached {
            return spawn_detached(name, cmd, labels);
        }
        return request_switch(&current, name);
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
            if detached || !allow_existing {
                eprintln!("error: session '{}' already exists", name);
                return 1;
            }
            match socket::session_connect(&path_str) {
                Ok(fd) => {
                    // Session already exists: apply any requested labels to the
                    // live daemon before attaching (create-time labels can't
                    // apply retroactively, so honor them here instead).
                    if !labels.is_empty() && apply_labels(name, labels) != 0 {
                        return 1;
                    }
                    return attach_and_follow_switches(cfg, name.to_string(), fd);
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
        return match daemon::spawn_daemon_detached(&cfg, cmd, labels) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {}", e);
                1
            }
        };
    }

    let socket_fd = match daemon::spawn_daemon(&cfg, cmd, labels) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };

    attach_and_follow_switches(cfg, name.to_string(), socket_fd)
}

/// Apply `labels` (a slice of "k=v ..." groups) to a running session by name.
/// Returns 0 on success, non-zero (after printing an error) otherwise.
fn apply_labels(name: &str, labels: &[String]) -> i32 {
    let payload = labels.join(" ");
    match label_request(name, Tag::LabelSet, payload.as_bytes(), Tag::Ack) {
        Ok(_) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

/// Spawn `name` in the background without attaching (used for `--detached`
/// even from inside a session).
fn spawn_detached(name: &str, cmd: &[String], labels: &[String]) -> i32 {
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
    match socket::session_exists(&cfg.socket_dir, &cfg.session_name) {
        Ok(true) => {
            eprintln!("error: session '{}' already exists", name);
            1
        }
        Ok(false) => match daemon::spawn_daemon_detached(&cfg, cmd, labels) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {}", e);
                1
            }
        },
        Err(e) => {
            eprintln!("error: {}", e);
            1
        }
    }
}

/// Ask the session we're currently inside (`current`) to switch the
/// interactive user over to `target`. The daemon relays the request to its
/// leader client, which detaches and re-attaches to `target`.
fn request_switch(current: &str, target: &str) -> i32 {
    // Reject an obviously invalid target up front so the user gets a clear
    // error instead of a silently ignored request on the daemon side.
    if let Err(e) = Cfg::resolve(target) {
        eprintln!("error: {}", e);
        return 1;
    }
    if target == current {
        // Already here — nothing to do.
        return 0;
    }
    let fd = match util::session_connect_by_name(current) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("error: {}", e);
            return 1;
        }
    };
    if let Err(e) = ipc::send(fd.as_raw_fd(), Tag::Switch, target.as_bytes()) {
        eprintln!("error: failed to request switch: {}", e);
        return 1;
    }
    0
}

/// Run the client against `fd`, and whenever the daemon tells us to switch to
/// another session, attach to that one instead — spawning it in the session's
/// reported cwd if it doesn't exist yet. Returns when the user finally detaches.
fn attach_and_follow_switches(mut cfg: Cfg, mut name: String, fd: OwnedFd) -> i32 {
    let mut fd = fd;
    loop {
        util::write_last_session(&cfg.socket_dir, &name);
        util::run_hook("RIFT_ON_ATTACH", &cfg.session_name);
        let (code, outcome) = daemon::run_client_outcome(fd);
        util::run_hook("RIFT_ON_DETACH", &cfg.session_name);

        let (next_name, cwd) = match outcome {
            daemon::ClientOutcome::Detached => return code,
            daemon::ClientOutcome::Switch { name, cwd } => (name, cwd),
        };

        match connect_or_spawn_for_switch(&next_name, cwd.as_deref()) {
            Ok((next_cfg, next_fd)) => {
                cfg = next_cfg;
                name = next_name;
                fd = next_fd;
            }
            Err(e) => {
                eprintln!("error: switch to '{}' failed: {}", next_name, e);
                return 1;
            }
        }
    }
}

/// Resolve `name`, connect to it if it exists, otherwise spawn a fresh session
/// rooted at `cwd` (the previous session's directory). Returns the resolved
/// config and a connected client socket.
fn connect_or_spawn_for_switch(name: &str, cwd: Option<&str>) -> Result<(Cfg, OwnedFd), String> {
    let cfg = Cfg::resolve(name)?;
    socket::ensure_dirs(&cfg.socket_dir).map_err(|e| format!("failed to create dirs: {}", e))?;
    let path_str = cfg
        .socket_path
        .to_str()
        .ok_or_else(|| "invalid socket path".to_string())?
        .to_string();

    if let Ok(true) = socket::session_exists(&cfg.socket_dir, &cfg.session_name)
        && let Ok(fd) = socket::session_connect(&path_str)
    {
        return Ok((cfg, fd));
    }
    socket::cleanup_stale_socket(&cfg.socket_dir, &cfg.session_name);

    // Spawn a new session in the previous session's cwd so the switch lands in
    // a sensible directory. Changing our own cwd is fine: this process only
    // forks the daemon (which inherits it) and then re-attaches.
    if let Some(cwd) = cwd
        && !cwd.is_empty()
    {
        let _ = std::env::set_current_dir(cwd);
    }
    let fd = daemon::spawn_daemon(&cfg, &[], &[])?;
    Ok((cfg, fd))
}

pub fn cmd_smart(program: &str, args: &[String], force_new: bool) -> i32 {
    let Some(base_name) = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
    else {
        eprintln!("error: cannot derive a session name from command '{program}'");
        return 1;
    };

    let executable = find_executable(program);
    if executable.is_none() && !force_new && !args.is_empty() && session_is_connectable(base_name) {
        return cmd_attach(base_name, false, &[], &[]);
    }
    if executable.is_none() && (force_new || !args.is_empty()) {
        eprintln!("error: command not found in PATH: {program}");
        return 1;
    }

    let command = executable.map_or_else(Vec::new, |path| {
        let mut command = Vec::with_capacity(args.len() + 1);
        command.push(path.to_string_lossy().into_owned());
        command.extend_from_slice(args);
        command
    });

    if !force_new {
        return cmd_attach(base_name, false, &command, &[]);
    }

    let Some(name) = next_command_session_name(base_name) else {
        eprintln!("error: could not allocate a session name for '{base_name}'");
        return 1;
    };
    cmd_attach_new(&name, &command)
}

fn find_executable(program: &str) -> Option<PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return is_executable(path).then(|| path.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn session_is_connectable(name: &str) -> bool {
    let Ok(cfg) = Cfg::resolve(name) else {
        return false;
    };
    let Some(path) = cfg.socket_path.to_str() else {
        return false;
    };
    socket::session_connect(path).is_ok()
}

fn next_command_session_name(base_name: &str) -> Option<String> {
    let prefix = socket::session_prefix();
    let socket_dir = socket::socket_dir();
    let mut index = 0usize;
    loop {
        let candidate = if index == 0 {
            base_name.to_string()
        } else {
            format!("{base_name}.{index}")
        };
        let full_name = socket::get_session_name(&prefix, &candidate).ok()?;
        if !socket::session_exists(&socket_dir, &full_name).ok()? {
            return Some(candidate);
        }
        index = index.checked_add(1)?;
    }
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
    cmd_attach(&bare, false, &[], &[])
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
        None => return cmd_pick(),
    };
    let prefix = socket::session_prefix();
    let full = format!("{}{}", prefix, name);
    match socket::session_exists(&socket_dir, &full) {
        Ok(true) => cmd_attach(&name, false, &[], &[]),
        _ => {
            eprintln!("last session '{}' is gone", name);
            util::clear_last_session(&socket_dir);
            cmd_pick()
        }
    }
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
