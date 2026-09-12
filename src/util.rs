use std::io::{self, Write};
use std::os::unix::io::{BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use nix::unistd;

use crate::ipc;
use crate::label;
use crate::socket;

// -- Interactive input detection ---------------------------------------------

#[derive(Default)]
struct UserInputDetector {
    found: bool,
}

impl vte::Perform for UserInputDetector {
    fn print(&mut self, _c: char) {
        self.found = true;
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\r' | b'\n' | b'\t' | 0x08) {
            self.found = true;
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            'u' => {
                let event_type = params
                    .iter()
                    .nth(1)
                    .and_then(|param| param.get(1))
                    .copied()
                    .unwrap_or(1);
                self.found = event_type != 3;
            }
            '~' | 'A'..='D' => self.found = true,
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if intermediates == b"O" && matches!(byte, b'A'..=b'D' | b'P'..=b'S') {
            self.found = true;
        }
    }

    fn terminated(&self) -> bool {
        self.found
    }
}

/// Return whether a terminal-input payload contains intentional keyboard input.
///
/// Mouse reports, focus events, terminal capability responses, and kitty key
/// release events do not count. The daemon uses this to transfer interactive
/// leadership without letting terminal-generated replies steal resize ownership.
pub fn is_user_input(payload: &[u8]) -> bool {
    let mut parser = vte::Parser::new();
    let mut detector = UserInputDetector::default();
    parser.advance(&mut detector, payload);
    detector.found
}

// -- Session listing ----------------------------------------------------------

pub struct SessionEntry {
    pub name: String,
    pub pid: Option<i32>,
    pub clients_len: Option<usize>,
    pub is_error: bool,
    pub error_name: Option<String>,
    pub cmd: Option<String>,
    pub cwd: Option<String>,
    pub created_at: u64,
    pub task_ended_at: Option<u64>,
    pub task_exit_code: Option<u8>,
    pub labels: Option<String>,
}

pub fn get_session_entries(socket_dir: &Path) -> io::Result<Vec<SessionEntry>> {
    let dir = std::fs::read_dir(socket_dir)?;
    let mut sessions = Vec::with_capacity(30);

    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };

        if name.ends_with(".ssh-auth-sock") {
            continue;
        }

        // Skip non-socket files (e.g. "logs" directory)
        match socket::session_exists(socket_dir, &name) {
            Ok(true) => {}
            _ => continue,
        }

        let socket_path = match socket::get_socket_path(socket_dir, &name) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let path_str = match socket_path.to_str() {
            Some(s) => s,
            None => continue,
        };

        match ipc::probe_session(path_str) {
            Ok(result) => {
                drop(result.fd);

                let cmd = if !result.info.cmd.is_empty() {
                    Some(String::from_utf8_lossy(&result.info.cmd).into_owned())
                } else {
                    None
                };
                let cwd = if !result.info.cwd.is_empty() {
                    Some(String::from_utf8_lossy(&result.info.cwd).into_owned())
                } else {
                    None
                };

                let task_ended_at = if result.info.task_ended_at > 0 {
                    Some(result.info.task_ended_at)
                } else {
                    None
                };
                let labels = result
                    .labels
                    .as_deref()
                    .map(|data| String::from_utf8_lossy(data).into_owned());

                sessions.push(SessionEntry {
                    name,
                    pid: Some(result.info.pid),
                    clients_len: Some(result.info.clients_len),
                    is_error: false,
                    error_name: None,
                    cmd,
                    cwd,
                    created_at: result.info.created_at,
                    task_ended_at,
                    task_exit_code: if task_ended_at.is_some() {
                        Some(result.info.task_exit_code)
                    } else {
                        None
                    },
                    labels,
                });
            }
            Err(ipc::ProbeError::ConnectionRefused) => {
                socket::cleanup_stale_socket(socket_dir, &name);
                sessions.push(SessionEntry {
                    name,
                    pid: None,
                    clients_len: None,
                    is_error: true,
                    error_name: Some("ConnectionRefused".into()),
                    cmd: None,
                    cwd: None,
                    created_at: 0,
                    task_ended_at: Some(0),
                    task_exit_code: Some(1),
                    labels: None,
                });
            }
            Err(ipc::ProbeError::Timeout) => {
                sessions.push(SessionEntry {
                    name,
                    pid: None,
                    clients_len: None,
                    is_error: true,
                    error_name: Some("Timeout".into()),
                    cmd: None,
                    cwd: None,
                    created_at: 0,
                    task_ended_at: Some(0),
                    task_exit_code: Some(1),
                    labels: None,
                });
            }
            Err(e) => {
                let err_name = format!("{}", e);
                sessions.push(SessionEntry {
                    name,
                    pid: None,
                    clients_len: None,
                    is_error: true,
                    error_name: Some(err_name),
                    cmd: None,
                    cwd: None,
                    created_at: 0,
                    task_ended_at: Some(0),
                    task_exit_code: Some(1),
                    labels: None,
                });
            }
        }
    }

    sessions.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(sessions)
}

pub fn write_session_line(
    w: &mut dyn Write,
    session: &SessionEntry,
    short: bool,
    verbose: bool,
    socket_dir: &Path,
    current_session: Option<&str>,
) -> io::Result<()> {
    let prefix = match current_session {
        Some(current) if current == session.name => "→ ",
        Some(_) => "  ",
        None => "",
    };

    if short {
        if session.is_error {
            return Ok(());
        }
        return writeln!(w, "{}", session.name);
    }

    if session.is_error {
        let err_name = session.error_name.as_deref().unwrap_or("Unknown");
        let status = if err_name == "ConnectionRefused" {
            "cleaning up"
        } else {
            "unreachable"
        };
        return writeln!(
            w,
            "{}name={}\terr={}\tstatus={}",
            prefix, session.name, err_name, status
        );
    }

    write!(
        w,
        "{}name={}\tpid={}\tclients={}\tcreated={}",
        prefix,
        session.name,
        session.pid.unwrap(),
        session.clients_len.unwrap(),
        session.created_at,
    )?;
    if let Some(ref cwd) = session.cwd {
        write!(w, "\tstart_dir={}", cwd)?;
    }
    if let Some(ref cmd) = session.cmd {
        write!(w, "\tcmd={}", cmd)?;
    }
    if let Some(ended_at) = session.task_ended_at
        && ended_at > 0
    {
        write!(w, "\tended={}", ended_at)?;
        if let Some(exit_code) = session.task_exit_code {
            write!(w, "\texit_code={}", exit_code)?;
        }
    }
    if let Some(labels) = session.labels.as_deref() {
        for (key, value) in label::decode(labels) {
            write!(w, "\t{}={}", key, value)?;
        }
    }
    if verbose {
        if session.created_at > 0 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if now >= session.created_at {
                write!(w, "\tuptime={}", format_duration(now - session.created_at))?;
            }
        }
        let log_path = socket_dir
            .join("logs")
            .join(format!("{}.log", session.name));
        write!(w, "\tlog={}", log_path.display())?;
    }
    writeln!(w)
}

fn format_duration(secs: u64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if d > 0 {
        format!("{}d{}h", d, h)
    } else if h > 0 {
        format!("{}h{}m", h, m)
    } else if m > 0 {
        format!("{}m{}s", m, s)
    } else {
        format!("{}s", s)
    }
}

// -- Session resolution helpers -----------------------------------------------

pub fn resolve_sessions(
    socket_dir: &std::path::Path,
    names: &[String],
) -> Result<Vec<String>, String> {
    let prefix = socket::session_prefix();
    let patterns: Vec<String> = names.iter().map(|n| format!("{}{}", prefix, n)).collect();
    let has_glob = patterns.iter().any(|p| p.ends_with('*'));

    if !has_glob {
        return Ok(patterns);
    }

    let entries = get_session_entries(socket_dir).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound {
            "no sessions found".to_string()
        } else {
            format!("{}", e)
        }
    })?;

    let matching: Vec<String> = entries
        .iter()
        .filter(|e| pattern_matches(&patterns, &e.name))
        .map(|e| e.name.clone())
        .collect();

    if matching.is_empty() {
        return Err("no matching sessions found".into());
    }
    Ok(matching)
}

pub fn pattern_matches(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| {
        if let Some(stem) = p.strip_suffix('*') {
            name.starts_with(stem)
        } else {
            name == p
        }
    })
}

// -- Lifecycle hooks ----------------------------------------------------------
//
// Users can set `RIFT_ON_ATTACH`, `RIFT_ON_DETACH`, `RIFT_ON_EXIT` to shell
// snippets that fire on the corresponding event. The snippet runs via `sh -c`
// with `$RIFT_SESSION` set and the session name also passed as `$1`. The hook
// is fire-and-forget (stdio is detached); users redirect inside the snippet
// if they want output. The daemon-side EXIT hook inherits the env that was
// present when the daemon was spawned by the first client.

pub fn run_hook(env_var: &str, session_name: &str) {
    let cmd = match std::env::var(env_var) {
        Ok(c) if !c.is_empty() => c,
        _ => return,
    };
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .arg("rift-hook")
        .arg(session_name)
        .env("RIFT_SESSION", session_name)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

// -- Last-session state file --------------------------------------------------
//
// We record the bare (pre-prefix) session name of the most recent successful
// attach in `<socket_dir>/.last`, so `rift last` can re-attach to it. Stored
// pre-prefix because `cmd_attach` re-applies the current `RIFT_SESSION_PREFIX`.

pub fn write_last_session(socket_dir: &Path, bare_name: &str) {
    let path = socket_dir.join(".last");
    let _ = std::fs::write(path, bare_name);
}

pub fn read_last_session(socket_dir: &Path) -> Option<String> {
    let path = socket_dir.join(".last");
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn clear_last_session(socket_dir: &Path) {
    let _ = std::fs::remove_file(socket_dir.join(".last"));
}

pub fn session_connect_by_name(name: &str) -> Result<OwnedFd, String> {
    let prefix = socket::session_prefix();
    let session_name = socket::get_session_name(&prefix, name).map_err(|e| format!("{}", e))?;
    let socket_dir = socket::socket_dir();
    let socket_path = socket::get_socket_path(&socket_dir, &session_name).map_err(|_| {
        socket::print_session_name_too_long(&session_name, &socket_dir);
        "socket path too long".to_string()
    })?;
    let path_str = socket_path.to_str().ok_or("invalid socket path")?;
    socket::session_connect(path_str)
        .map_err(|e| format!("cannot connect to session '{}': {}", name, e))
}

// -- Shell quoting ------------------------------------------------------------

pub fn shell_needs_quoting(arg: &str) -> bool {
    if arg.is_empty() {
        return true;
    }
    arg.bytes().any(|ch| {
        matches!(
            ch,
            b' ' | b'\t'
                | b'"'
                | b'\''
                | b'\\'
                | b'$'
                | b'`'
                | b'!'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b'|'
                | b'&'
                | b';'
                | b'<'
                | b'>'
                | b'?'
                | b'*'
                | b'~'
                | b'#'
                | b'\n'
        )
    })
}

pub fn shell_quote(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

// -- Device Attributes responses ----------------------------------------------

const DA1_QUERY: &[u8] = b"\x1b[c";
const DA1_QUERY_EXPLICIT: &[u8] = b"\x1b[0c";
const DA2_QUERY: &[u8] = b"\x1b[>c";
const DA2_QUERY_EXPLICIT: &[u8] = b"\x1b[>0c";
const DA1_RESPONSE: &[u8] = b"\x1b[?62;22c";
const DA2_RESPONSE: &[u8] = b"\x1b[>1;10;0c";

/// Scan PTY output for DA queries and respond on behalf of the terminal.
/// Handles the case where no client is attached (e.g. rift run) and the shell
/// sends a DA query that would otherwise go unanswered.
pub fn respond_to_device_attributes(pty_fd: RawFd, data: &[u8]) {
    let responses = device_attribute_responses(data);
    if responses.is_empty() {
        return;
    }
    let bfd = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    let _ = unistd::write(bfd, &responses);
}

/// Build terminal capability responses for DA queries found in output.
///
/// Interactive clients normally let the real terminal answer these. Headless
/// clients such as `rift run` send these bytes back as PTY input so shell
/// startup does not stall waiting for a terminal.
pub fn device_attribute_responses(data: &[u8]) -> Vec<u8> {
    let mut responses = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            // Skip DA responses (contain '?' after CSI)
            if i + 2 < data.len() && data[i + 2] == b'?' {
                i += 3;
                continue;
            }
            if data[i..].starts_with(DA2_QUERY) || data[i..].starts_with(DA2_QUERY_EXPLICIT) {
                responses.extend_from_slice(DA2_RESPONSE);
            } else if data[i..].starts_with(DA1_QUERY) || data[i..].starts_with(DA1_QUERY_EXPLICIT)
            {
                responses.extend_from_slice(DA1_RESPONSE);
            }
        }
        i += 1;
    }
    responses
}

// -- Task exit markers --------------------------------------------------------

const TASK_MARKER: &[u8] = b"RIFT_TASK_REQUEST_COMPLETED:";
const TASK_SCAN_LIMIT: usize = 1024;

/// Extract the request ID embedded in a wrapped `Run` command.
///
/// Keeping the command itself as the `Run` payload preserves compatibility
/// with daemons started by older rift binaries.
pub fn task_request_id(command: &[u8]) -> Option<u64> {
    let marker_start = command
        .windows(TASK_MARKER.len())
        .position(|window| window == TASK_MARKER)?;
    let id_start = marker_start + TASK_MARKER.len();
    let id_end = command[id_start..]
        .iter()
        .position(|&byte| byte == b':')
        .map(|offset| id_start + offset)?;
    std::str::from_utf8(&command[id_start..id_end])
        .ok()?
        .parse()
        .ok()
}

/// Incrementally scan PTY output for task completion records.
///
/// Records have the form
/// `RIFT_TASK_REQUEST_COMPLETED:<request_id>:<exit_code>` and must end in CR
/// or LF. Keeping a small carry buffer makes detection robust when a record is
/// split across PTY reads. Invalid occurrences are skipped; this matters
/// because an interactive shell may echo the `printf` command containing the
/// marker before emitting the actual completion record.
pub fn scan_task_completions(carry: &mut Vec<u8>, output: &[u8]) -> Vec<(u64, u8)> {
    carry.extend_from_slice(output);
    let mut completions = Vec::new();
    let mut search_from = 0;
    let mut consumed = 0;

    while search_from + TASK_MARKER.len() <= carry.len() {
        let Some(relative) = carry[search_from..]
            .windows(TASK_MARKER.len())
            .position(|window| window == TASK_MARKER)
        else {
            break;
        };
        let marker_start = search_from + relative;
        let record_start = marker_start + TASK_MARKER.len();
        let Some(relative_end) = carry[record_start..]
            .iter()
            .position(|&byte| byte == b'\n' || byte == b'\r')
        else {
            consumed = marker_start;
            break;
        };
        let record_end = record_start + relative_end;
        let record = &carry[record_start..record_end];

        if let Some(colon) = record.iter().position(|&byte| byte == b':') {
            let request = std::str::from_utf8(&record[..colon])
                .ok()
                .and_then(|value| value.parse::<u64>().ok());
            let code = std::str::from_utf8(&record[colon + 1..])
                .ok()
                .and_then(|value| value.parse::<u8>().ok());
            if let (Some(request), Some(code)) = (request, code) {
                completions.push((request, code));
            }
        }

        search_from = record_end + 1;
        consumed = search_from;
    }

    if consumed > 0 {
        carry.drain(..consumed);
    }
    if carry.len() > TASK_SCAN_LIMIT {
        let keep = TASK_MARKER.len().saturating_sub(1);
        carry.drain(..carry.len().saturating_sub(keep));
    }

    completions
}

// -- Kitty keyboard protocol --------------------------------------------------

/// Detect Kitty keyboard protocol Ctrl+\ key events anywhere in `buf`.
///
/// The full event format is:
///   CSI keycode[:alt[:base]][;modifiers[:event-type[:text-cps]][;text-as-cps]] u
///
/// With richer flags enabled by inner programs (report alternate keys = 4,
/// report associated text = 16, report event types = 8), the same Ctrl+\ keypress
/// can arrive in many shapes — `\x1b[92;5u`, `\x1b[92;5:1u`, `\x1b[92:X;5u`,
/// `\x1b[92;5;92u`, etc. We accept any sequence whose first numeric field
/// (before an optional `:`) is 92 and whose second numeric field (before an
/// optional `:`) is 5 — that's "backslash, ctrl held," regardless of the
/// alternate-keys / event-type / associated-text trimmings.
pub fn is_kitty_ctrl_backslash(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 < buf.len() {
        if buf[i] != 0x1b || buf[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let body_start = i + 2;
        let mut j = body_start;
        while j < buf.len() && buf[j] != b'u' && buf[j] != 0x1b {
            j += 1;
        }
        if j >= buf.len() || buf[j] != b'u' {
            i += 1;
            continue;
        }
        let body = &buf[body_start..j];
        if kitty_event_is_ctrl_backslash(body) {
            return true;
        }
        i = j + 1;
    }
    false
}

fn kitty_event_is_ctrl_backslash(body: &[u8]) -> bool {
    let mut fields = body.split(|&b| b == b';');
    let first = match fields.next() {
        Some(f) => f,
        None => return false,
    };
    let second = match fields.next() {
        Some(f) => f,
        None => return false,
    };
    leading_number(first) == Some(92) && leading_number(second) == Some(5)
}

fn leading_number(field: &[u8]) -> Option<u32> {
    let end = field.iter().position(|&b| b == b':').unwrap_or(field.len());
    std::str::from_utf8(&field[..end]).ok()?.parse().ok()
}

/// Force OSC 133 prompt markers to tell the outer terminal not to redraw.
///
/// Kitty shell integration otherwise clears prompt lines after resize while
/// the real shell redraw travels through the daemon with inner-PTY coordinates.
pub fn rewrite_prompt_redraw(data: &[u8]) -> Option<Vec<u8>> {
    const MARKER: &[u8] = b"\x1b]133;A";

    if !data.windows(MARKER.len()).any(|window| window == MARKER) {
        return None;
    }

    let mut result = data.to_vec();
    let mut search_end = result.len();
    let mut changed = false;

    while search_end > 0 {
        let Some(position) = result[..search_end]
            .windows(MARKER.len())
            .rposition(|window| window == MARKER)
        else {
            break;
        };
        search_end = position;

        let params_start = position + MARKER.len();
        let Some(terminator) = find_osc_terminator(&result, params_start) else {
            continue;
        };
        let params = &result[params_start..terminator];

        if params
            .windows(b"redraw=0".len())
            .any(|window| window == b"redraw=0")
        {
            continue;
        }

        if let Some(offset) = params
            .windows(b"redraw=".len())
            .position(|window| window == b"redraw=")
        {
            let value_start = params_start + offset + b"redraw=".len();
            let value_end = result[value_start..terminator]
                .iter()
                .position(|byte| *byte == b';')
                .map_or(terminator, |offset| value_start + offset);
            result.splice(value_start..value_end, *b"0");
        } else {
            result.splice(terminator..terminator, *b";redraw=0");
        }
        changed = true;
    }

    changed.then_some(result)
}

fn find_osc_terminator(data: &[u8], start: usize) -> Option<usize> {
    let mut index = start;
    while index < data.len() {
        if data[index] == 0x07 || (data[index] == 0x1b && data.get(index + 1) == Some(&b'\\')) {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Serialize terminal contents in the requested format.
pub fn serialize_terminal(
    term: &crate::term_state::TermState,
    format: ipc::HistoryFormat,
) -> Option<Vec<u8>> {
    let data = match format {
        ipc::HistoryFormat::Plain => term.contents_plain(),
        ipc::HistoryFormat::Vt => term.contents_vt(),
        ipc::HistoryFormat::Html => term.contents_html(),
    };
    if data.is_empty() { None } else { Some(data) }
}

// -- Shell detection ----------------------------------------------------------

pub fn detect_shell() -> String {
    std::env::var("RIFT_SHELL")
        .or_else(|_| std::env::var("SHELL"))
        .unwrap_or_else(|_| "/bin/sh".into())
}

pub fn filter_tail_output(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            out.push(data[i]);
            i += 1;
            continue;
        }

        if i + 1 >= data.len() {
            // Truncated ESC at end of payload — drop it.
            break;
        }

        match data[i + 1] {
            b'[' => {
                // CSI: ESC [ params... final (0x40..=0x7E)
                let mut j = i + 2;
                while j < data.len() && !(0x40..=0x7E).contains(&data[j]) {
                    j += 1;
                }
                if j >= data.len() {
                    break;
                }
                let final_byte = data[j];
                if final_byte == b'm' || final_byte == b'K' {
                    out.extend_from_slice(&data[i..=j]);
                }
                i = j + 1;
            }
            b']' | b'P' | b'X' | b'^' | b'_' => {
                // OSC / DCS / SOS / PM / APC: terminated by ST (ESC \) or BEL.
                let mut j = i + 2;
                while j < data.len() {
                    if data[j] == 0x07 {
                        j += 1;
                        break;
                    }
                    if data[j] == 0x1b && j + 1 < data.len() && data[j + 1] == b'\\' {
                        j += 2;
                        break;
                    }
                    j += 1;
                }
                i = j;
            }
            b'O' => {
                // SS3: ESC O final
                i = (i + 3).min(data.len());
            }
            _ => {
                // Other two-byte ESC sequence (ESC =, ESC >, ESC 7/8, ESC D/E/M, ...).
                i += 2;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_completion_scanner_handles_split_records() {
        let mut carry = Vec::new();
        assert!(scan_task_completions(&mut carry, b"output\nRIFT_TASK_REQUEST_COMP").is_empty());
        assert_eq!(
            scan_task_completions(&mut carry, b"LETED:42:7\r\nprompt"),
            vec![(42, 7)]
        );
    }

    #[test]
    fn task_request_id_is_extracted_from_wrapped_command() {
        assert_eq!(
            task_request_id(b"printf 'RIFT_TASK_REQUEST_COMPLETED:1234:%d\\n' \"$status\""),
            Some(1234)
        );
    }

    #[test]
    fn task_completion_scanner_skips_echoed_printf_and_finds_multiple_records() {
        let mut carry = Vec::new();
        let output = concat!(
            "printf '\\nRIFT_TASK_REQUEST_COMPLETED:19:%d\\n' \"$status\"\r\n",
            "\r\nRIFT_TASK_REQUEST_COMPLETED:19:0\r\n",
            "RIFT_TASK_REQUEST_COMPLETED:20:127\n"
        );
        assert_eq!(
            scan_task_completions(&mut carry, output.as_bytes()),
            vec![(19, 0), (20, 127)]
        );
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("it's safe"), "'it'\\''s safe'");
    }

    #[test]
    fn terminal_tail_filter_preserves_color_but_removes_cursor_motion() {
        assert_eq!(
            filter_tail_output(b"\x1b[31mred\x1b[0m\x1b[2Aup"),
            b"\x1b[31mred\x1b[0mup"
        );
    }

    #[test]
    fn device_attribute_queries_produce_terminal_responses() {
        assert_eq!(
            device_attribute_responses(b"one\x1b[c two\x1b[>0c"),
            [DA1_RESPONSE, DA2_RESPONSE].concat()
        );
    }

    #[test]
    fn user_input_detector_ignores_terminal_generated_events() {
        assert!(!is_user_input(b"\x1b[<0;12;4M"));
        assert!(!is_user_input(b"\x1b[I"));
        assert!(!is_user_input(b"\x1b[?1;2c"));
        assert!(!is_user_input(b"\x1b[102;1:3u"));
    }

    #[test]
    fn user_input_detector_accepts_keyboard_input() {
        assert!(is_user_input(b"text"));
        assert!(is_user_input(b"\r"));
        assert!(is_user_input(b"\x1b[A"));
        assert!(is_user_input(b"\x1b[102;1:1u"));
        assert!(is_user_input(b"\x1b[200~pasted\x1b[201~"));
    }

    #[test]
    fn prompt_redraw_is_added_or_replaced() {
        assert_eq!(
            rewrite_prompt_redraw(b"\x1b]133;A\x07"),
            Some(b"\x1b]133;A;redraw=0\x07".to_vec())
        );
        assert_eq!(
            rewrite_prompt_redraw(b"\x1b]133;A;aid=2;redraw=last\x1b\\"),
            Some(b"\x1b]133;A;aid=2;redraw=0\x1b\\".to_vec())
        );
        assert_eq!(rewrite_prompt_redraw(b"\x1b]133;A;redraw=0\x07"), None);
    }
}
