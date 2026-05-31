use std::io::{self, Write};
use std::os::unix::io::{BorrowedFd, OwnedFd, RawFd};
use std::path::Path;

use nix::unistd;

use crate::ipc;
use crate::socket;

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
    if let Some(ended_at) = session.task_ended_at {
        if ended_at > 0 {
            write!(w, "\tended={}", ended_at)?;
            if let Some(exit_code) = session.task_exit_code {
                write!(w, "\texit_code={}", exit_code)?;
            }
        }
    }
    writeln!(w)
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
    let bfd = unsafe { BorrowedFd::borrow_raw(pty_fd) };
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            // Skip DA responses (contain '?' after CSI)
            if i + 2 < data.len() && data[i + 2] == b'?' {
                i += 3;
                continue;
            }
            if data[i..].starts_with(DA2_QUERY) || data[i..].starts_with(DA2_QUERY_EXPLICIT) {
                let _ = unistd::write(&bfd, DA2_RESPONSE);
            } else if data[i..].starts_with(DA1_QUERY) || data[i..].starts_with(DA1_QUERY_EXPLICIT)
            {
                let _ = unistd::write(&bfd, DA1_RESPONSE);
            }
        }
        i += 1;
    }
}

// -- Task exit markers --------------------------------------------------------

const TASK_MARKER: &str = "RIFT_TASK_COMPLETED:";

pub fn find_task_exit_marker(output: &[u8]) -> Option<u8> {
    let marker = TASK_MARKER.as_bytes();
    if let Some(idx) = output.windows(marker.len()).position(|w| w == marker) {
        let after = &output[idx + marker.len()..];
        let end = after
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
            .unwrap_or(after.len());
        let code_str = std::str::from_utf8(&after[..end]).ok()?;
        match code_str.parse::<u8>() {
            Ok(code) => Some(code),
            Err(_) => {
                log::warn!("failed to parse task exit code from: {}", code_str);
                None
            }
        }
    } else {
        None
    }
}

// -- Kitty keyboard protocol --------------------------------------------------

/// Detects Kitty keyboard protocol escape sequence for Ctrl+\
/// 92 = backslash, 5 = ctrl modifier, :1 = key press event
pub fn is_kitty_ctrl_backslash(buf: &[u8]) -> bool {
    buf.windows(7).any(|w| w == b"\x1b[92;5u") || buf.windows(9).any(|w| w == b"\x1b[92;5:1u")
}

// -- Terminal serialization (vt100 crate) -------------------------------------

/// Serialize the current terminal state for reattach.
/// Returns the VT escape sequences needed to reproduce the screen.
pub fn serialize_terminal_state(parser: &vt100::Parser) -> Option<Vec<u8>> {
    let data = parser.screen().state_formatted();
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HistoryFormat {
    Plain = 0,
    Vt = 1,
    Html = 2,
}

/// Serialize terminal contents in the requested format.
pub fn serialize_terminal(parser: &vt100::Parser, format: HistoryFormat) -> Option<Vec<u8>> {
    let screen = parser.screen();
    let data = match format {
        HistoryFormat::Plain => screen.contents().into_bytes(),
        HistoryFormat::Vt => screen.contents_formatted(),
        HistoryFormat::Html => {
            // vt100 crate doesn't have built-in HTML export;
            // build a simple one from cell-by-cell iteration
            serialize_html(screen)
        }
    };
    if data.is_empty() {
        None
    } else {
        Some(data)
    }
}

fn serialize_html(screen: &vt100::Screen) -> Vec<u8> {
    let (rows, cols) = screen.size();
    let mut html = String::new();
    html.push_str("<pre>");
    for row in 0..rows {
        for col in 0..cols {
            let cell = screen.cell(row, col);
            if let Some(cell) = cell {
                let ch = cell.contents();
                if ch.is_empty() {
                    html.push(' ');
                } else {
                    // Escape HTML entities
                    for c in ch.chars() {
                        match c {
                            '<' => html.push_str("&lt;"),
                            '>' => html.push_str("&gt;"),
                            '&' => html.push_str("&amp;"),
                            '"' => html.push_str("&quot;"),
                            _ => html.push(c),
                        }
                    }
                }
            } else {
                html.push(' ');
            }
        }
        html.push('\n');
    }
    html.push_str("</pre>");
    html.into_bytes()
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
