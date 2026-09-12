//! Terminal-state backend built on `alacritty_terminal`.
//!
//! This is the replacement for the `vt100` crate. It models the full screen
//! grid (with scrollback), alternate-screen buffer, cursor, and the handful
//! of DEC private modes rift needs to restore on reattach. The public surface
//! is intentionally the small "seam" the daemon and `util` serializers use:
//! `process`, `resize`, `size`, `alternate_screen`, plus the serialization
//! entry points implemented here (`serialize_state`, `contents_plain`,
//! `contents_html`).

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use std::cell::RefCell;
use std::rc::Rc;

/// A concrete `Dimensions` value for constructing / resizing the terminal.
#[derive(Debug, Clone, Copy)]
struct Size {
    rows: usize,
    cols: usize,
}

impl alacritty_terminal::grid::Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Scrollback capacity, aligned with current zmx/ghostty defaults.
const SCROLLBACK: usize = 10_000;

/// Shared window title, updated by the terminal's OSC 0/2 handling.
///
/// alacritty reports title changes via the `EventListener`; we capture the
/// latest value here so `serialize_state` can replay it on reattach.
#[derive(Clone, Default)]
struct TitleListener {
    title: Rc<RefCell<Option<String>>>,
}

impl EventListener for TitleListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => *self.title.borrow_mut() = Some(title),
            Event::ResetTitle => *self.title.borrow_mut() = None,
            _ => {}
        }
    }
}

/// Owns the alacritty terminal model plus the byte-stream parser that feeds it.
pub struct TermState {
    parser: Processor,
    term: Term<TitleListener>,
    title: Rc<RefCell<Option<String>>>,
    /// Working directory reported via OSC 7 (`file://host/path`), if any.
    cwd: Option<String>,
}

impl TermState {
    pub fn new(rows: u16, cols: u16) -> Self {
        let size = Size {
            rows: rows as usize,
            cols: cols as usize,
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        let listener = TitleListener::default();
        let title = listener.title.clone();
        Self {
            parser: Processor::new(),
            term: Term::new(config, &size, listener),
            title,
            cwd: None,
        }
    }

    /// Feed raw PTY bytes into the terminal model.
    pub fn process(&mut self, data: &[u8]) {
        // vte 0.14 does not surface OSC 7 (current working directory), so scan
        // the stream for it ourselves before handing bytes to the parser.
        if let Some(cwd) = scan_osc7_cwd(data) {
            self.cwd = Some(cwd);
        }
        self.parser.advance(&mut self.term, data);
    }

    /// Latest working directory reported via OSC 7, if the shell emits it.
    pub fn cwd(&self) -> Option<String> {
        self.cwd.as_deref().and_then(local_file_uri_path)
    }

    /// Complete OSC 7 URI, including its host. This is what `list` and terminal
    /// replay expose so remote SSH working directories remain distinguishable.
    pub fn cwd_uri(&self) -> Option<String> {
        self.cwd.clone()
    }

    pub fn focus_reporting(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::FOCUS_IN_OUT)
    }

    /// Latest window title reported via OSC 0/2, if any.
    ///
    /// Part of the backend seam; `serialize_state` replays the title directly,
    /// so the daemon does not yet query it.
    #[allow(dead_code)]
    pub fn title(&self) -> Option<String> {
        self.title.borrow().clone()
    }

    /// Current viewport size as `(rows, cols)`.
    ///
    /// Part of the backend seam; used by tests and available to the daemon for
    /// size queries (the daemon currently tracks size via `apply_resize`).
    #[allow(dead_code)]
    pub fn size(&self) -> (u16, u16) {
        (self.term.screen_lines() as u16, self.term.columns() as u16)
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.term.resize(Size {
            rows: rows as usize,
            cols: cols as usize,
        });
    }

    /// Whether the alternate screen is currently active.
    ///
    /// Part of the backend seam; `serialize_state` already handles alt-screen
    /// internally, so the daemon does not yet call this directly.
    #[allow(dead_code)]
    pub fn alternate_screen(&self) -> bool {
        self.term
            .mode()
            .contains(alacritty_terminal::term::TermMode::ALT_SCREEN)
    }

    // -- serialization --------------------------------------------------------

    /// Serialize the current terminal into VT escape sequences that reproduce
    /// it when replayed to a fresh terminal on reattach.
    ///
    /// For the primary screen this also replays the scrollback history: the
    /// history rows are streamed ahead of the visible rows with line feeds, so
    /// they scroll into the receiving terminal's own scrollback (matching zmx,
    /// which restores scrollback on reattach). The alternate screen has no
    /// scrollback, so it is painted with absolute cursor positioning instead.
    ///
    /// Returns `None` if the terminal is effectively empty.
    pub fn serialize_state(&self) -> Option<Vec<u8>> {
        let mode = self.term.mode();
        let alt = mode.contains(alacritty_terminal::term::TermMode::ALT_SCREEN);

        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();

        let mut out: Vec<u8> = Vec::new();
        let mut any_content = false;
        let mut cur_style = Style::default();

        if alt {
            // Enter the alternate screen, then paint the visible grid with
            // absolute positioning (sparse content lands on the right rows).
            out.extend_from_slice(b"\x1b[?1049h");
            out.extend_from_slice(b"\x1b[2J\x1b[H");
            for row in 0..rows {
                out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());
                any_content |= emit_row(&mut out, grid, Line(row as i32), cols, &mut cur_style);
            }
            out.extend_from_slice(b"\x1b[0m");
        } else {
            // Stream scrollback history followed by the visible rows. Each row
            // ends with CR/LF so that, once the receiver's screen fills, further
            // line feeds scroll the history off the top into its scrollback and
            // leave exactly the visible rows on screen. No `ED` clear here: on
            // the primary screen alacritty pushes the cleared viewport into
            // scrollback, which would prepend a spurious blank history line.
            out.extend_from_slice(b"\x1b[H");
            let top = -(grid.history_size() as i32);
            let last = rows as i32 - 1;
            for line in top..=last {
                if cur_style != Style::default() {
                    out.extend_from_slice(b"\x1b[0m");
                    cur_style = Style::default();
                }
                any_content |= emit_row(&mut out, grid, Line(line), cols, &mut cur_style);
                // Erase any stale tail on this row of the receiver without
                // moving the cursor (so we never force a wrap).
                out.extend_from_slice(b"\x1b[0m\x1b[K");
                cur_style = Style::default();
                if line != last {
                    out.extend_from_slice(b"\r\n");
                }
            }
            out.extend_from_slice(b"\x1b[0m");
        }

        // Restore DEC private modes rift cares about.
        if mode.contains(alacritty_terminal::term::TermMode::APP_CURSOR) {
            out.extend_from_slice(b"\x1b[?1h");
        }
        if mode.contains(alacritty_terminal::term::TermMode::APP_KEYPAD) {
            out.extend_from_slice(b"\x1b=");
        }
        if mode.contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE) {
            out.extend_from_slice(b"\x1b[?2004h");
        }

        // Restore mouse tracking. This matters because the client blanket-
        // disables every mouse mode at attach (`TERMINAL_RESET` in
        // `daemon/client.rs`) to clear modes a *previous* session left sticky on
        // the local terminal. Without re-enabling them here, reattaching to a
        // session running a mouse-aware program (vim, fzf, less, a pager) left
        // that program blind to the mouse: the inner program still believed it
        // had tracking on — so it never re-sent the enable — while the actual
        // terminal had it off. Clicks and wheel events then fell through to the
        // emulator, which is why "scrolling stopped working after reattach".
        //
        // Order matters: the tracking mode (1000/1002/1003) is emitted before
        // the encoding (1006/1005/1015), matching how applications enable them,
        // since some terminals reset the encoding when the mode changes.
        for (flag, seq) in [
            (
                alacritty_terminal::term::TermMode::MOUSE_REPORT_CLICK,
                &b"\x1b[?1000h"[..],
            ),
            (
                alacritty_terminal::term::TermMode::MOUSE_DRAG,
                &b"\x1b[?1002h"[..],
            ),
            (
                alacritty_terminal::term::TermMode::MOUSE_MOTION,
                &b"\x1b[?1003h"[..],
            ),
            (
                alacritty_terminal::term::TermMode::UTF8_MOUSE,
                &b"\x1b[?1005h"[..],
            ),
            (
                alacritty_terminal::term::TermMode::SGR_MOUSE,
                &b"\x1b[?1006h"[..],
            ),
            (
                alacritty_terminal::term::TermMode::FOCUS_IN_OUT,
                &b"\x1b[?1004h"[..],
            ),
        ] {
            if mode.contains(flag) {
                out.extend_from_slice(seq);
            }
        }

        // Restore the window title (OSC 2) and working directory (OSC 7) so a
        // reattaching terminal shows the same tab title / cwd as the original.
        let title = self.title.borrow();
        if let Some(title) = title.as_deref() {
            out.extend_from_slice(b"\x1b]2;");
            out.extend_from_slice(title.as_bytes());
            out.extend_from_slice(b"\x1b\\");
        }
        if let Some(cwd) = self.cwd.as_deref() {
            out.extend_from_slice(b"\x1b]7;");
            out.extend_from_slice(cwd.as_bytes());
            out.extend_from_slice(b"\x1b\\");
        }
        let has_meta = title.is_some() || self.cwd.is_some();

        // Restore cursor position (absolute within the visible screen).
        let cursor = grid.cursor.point;
        out.extend_from_slice(
            format!("\x1b[{};{}H", cursor.line.0 + 1, cursor.column.0 + 1).as_bytes(),
        );

        // Hide cursor if the source had it hidden.
        if !mode.contains(alacritty_terminal::term::TermMode::SHOW_CURSOR) {
            out.extend_from_slice(b"\x1b[?25l");
        }

        if any_content || alt || has_meta {
            Some(out)
        } else {
            None
        }
    }

    /// Plain-text dump of the visible screen (for `history`). Trailing blank
    /// lines are trimmed, matching the previous vt100 `contents()` behavior so
    /// consumers that take the "last N lines" see real content.
    pub fn contents_plain(&self) -> Vec<u8> {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let mut lines: Vec<String> = Vec::with_capacity(rows);
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                let cell = &grid[Line(row as i32)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                line.push(cell.c);
            }
            lines.push(line.trim_end().to_string());
        }
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        let mut s = lines.join("\n");
        if !s.is_empty() {
            s.push('\n');
        }
        s.into_bytes()
    }

    /// VT-formatted dump of the visible screen (for `history --format vt`):
    /// styled cells with SGR sequences, one row per line.
    pub fn contents_vt(&self) -> Vec<u8> {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let mut out: Vec<u8> = Vec::new();
        let mut cur_style = Style::default();
        for row in 0..rows {
            emit_row(&mut out, grid, Line(row as i32), cols, &mut cur_style);
            if cur_style != Style::default() {
                out.extend_from_slice(b"\x1b[0m");
                cur_style = Style::default();
            }
            out.push(b'\n');
        }
        out
    }

    /// HTML dump of the visible screen (for `history --format html`).
    ///
    /// Unlike the old vt100 exporter this preserves per-cell foreground and
    /// background color and bold/italic/underline, wrapping runs of like-styled
    /// cells in `<span style=...>`.
    pub fn contents_html(&self) -> Vec<u8> {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let mut html = String::from("<pre>");
        let mut open_style: Option<Style> = None;
        for row in 0..rows {
            for col in 0..cols {
                let cell = &grid[Line(row as i32)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                let style = Style::from_cell(cell);
                if open_style != Some(style) {
                    if open_style.is_some() {
                        html.push_str("</span>");
                    }
                    if style != Style::default() {
                        html.push_str(&style.html_span());
                        open_style = Some(style);
                    } else {
                        open_style = None;
                    }
                }
                push_html_char(&mut html, cell.c);
            }
            if open_style.is_some() {
                html.push_str("</span>");
                open_style = None;
            }
            html.push('\n');
        }
        html.push_str("</pre>");
        html.into_bytes()
    }
}

/// Scan a byte stream for the most recent OSC 7 working-directory report and
/// return the decoded path. OSC 7 has the form
/// `ESC ] 7 ; file://host/path ST` (ST = BEL or `ESC \`).
fn scan_osc7_cwd(data: &[u8]) -> Option<String> {
    const PREFIX: &[u8] = b"\x1b]7;";
    let mut result = None;
    let mut search_from = 0;
    while let Some(rel) = data[search_from..]
        .windows(PREFIX.len())
        .position(|w| w == PREFIX)
    {
        let body_start = search_from + rel + PREFIX.len();
        // Find the OSC terminator: BEL (0x07) or ST (ESC \).
        let mut end = body_start;
        while end < data.len() {
            if data[end] == 0x07 {
                break;
            }
            if data[end] == 0x1b && data.get(end + 1) == Some(&b'\\') {
                break;
            }
            end += 1;
        }
        if let Ok(uri) = std::str::from_utf8(&data[body_start..end])
            && uri.starts_with("file://")
        {
            result = Some(uri.to_string());
        }
        search_from = end.max(body_start);
    }
    result
}

/// Return a decoded path only when an OSC 7 URI identifies this machine.
/// Remote hosts are intentionally not treated as local filesystem paths.
fn local_file_uri_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let slash = rest.find('/')?;
    let host = &rest[..slash];
    let local_host = std::env::var("HOSTNAME").ok().or_else(|| {
        let mut buf = [0u8; 256];
        (unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0).then(|| {
            let len = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).into_owned()
        })
    });
    if !host.is_empty() && host != "localhost" && local_host.as_deref() != Some(host) {
        return None;
    }
    Some(percent_decode(&rest[slash..]))
}

/// Minimal percent-decoding for OSC 7 paths (`%20` etc.).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape one character into an HTML buffer.
fn push_html_char(html: &mut String, c: char) {
    match c {
        '<' => html.push_str("&lt;"),
        '>' => html.push_str("&gt;"),
        '&' => html.push_str("&amp;"),
        '"' => html.push_str("&quot;"),
        _ => html.push(c),
    }
}

/// Emit one grid row into `out`, updating `cur_style` as attributes change.
///
/// Trailing cells that are blank *and* default-styled are dropped so a full
/// width row doesn't force a wrap (which would desync the newline-driven
/// scrollback replay). Returns whether the row contained any content.
fn emit_row(
    out: &mut Vec<u8>,
    grid: &alacritty_terminal::grid::Grid<Cell>,
    line: Line,
    cols: usize,
    cur_style: &mut Style,
) -> bool {
    let row = &grid[line];

    // Find the last cell that carries visible content or non-default styling.
    let mut last = None;
    for col in 0..cols {
        let cell = &row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        if cell.c != ' ' || Style::from_cell(cell) != Style::default() {
            last = Some(col);
        }
    }
    let Some(last) = last else {
        return false;
    };

    for col in 0..=last {
        let cell = &row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let style = Style::from_cell(cell);
        if style != *cur_style {
            out.extend_from_slice(&style.sgr());
            *cur_style = style;
        }
        let mut buf = [0u8; 4];
        out.extend_from_slice(cell.c.encode_utf8(&mut buf).as_bytes());
    }
    true
}

/// Minimal cell style we serialize (fg/bg + a few attributes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Named(NamedColor::Foreground),
            bg: Color::Named(NamedColor::Background),
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
        }
    }
}

impl Style {
    fn from_cell(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.contains(Flags::UNDERLINE),
            inverse: cell.flags.contains(Flags::INVERSE),
        }
    }

    /// Build the SGR sequence to switch into this style from a clean slate.
    /// Always resets first (`0`) so we never inherit stale attributes.
    fn sgr(&self) -> Vec<u8> {
        let mut params: Vec<String> = vec!["0".to_string()];
        if self.bold {
            params.push("1".to_string());
        }
        if self.italic {
            params.push("3".to_string());
        }
        if self.underline {
            params.push("4".to_string());
        }
        if self.inverse {
            params.push("7".to_string());
        }
        append_color(&mut params, self.fg, true);
        append_color(&mut params, self.bg, false);
        format!("\x1b[{}m", params.join(";")).into_bytes()
    }

    /// Build an opening `<span style="...">` tag reflecting this style.
    fn html_span(&self) -> String {
        let mut css: Vec<String> = Vec::new();
        if let Some(fg) = css_color(self.fg, true) {
            css.push(format!("color:{fg}"));
        }
        if let Some(bg) = css_color(self.bg, false) {
            css.push(format!("background-color:{bg}"));
        }
        if self.bold {
            css.push("font-weight:bold".to_string());
        }
        if self.italic {
            css.push("font-style:italic".to_string());
        }
        if self.underline {
            css.push("text-decoration:underline".to_string());
        }
        format!("<span style=\"{}\">", css.join(";"))
    }
}

/// Map an alacritty color to a CSS color string, or `None` for the terminal
/// default. `inverse` handling is intentionally left to the SGR path; HTML
/// export renders explicit fg/bg only.
fn css_color(color: Color, fg: bool) -> Option<String> {
    match color {
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => None,
        Color::Named(named) => named_rgb(named).map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Spec(rgb) => Some(format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b)),
        // Indexed colors beyond the named 16 aren't mapped to a palette here;
        // fall back to the terminal default so text stays legible.
        Color::Indexed(i) => {
            let _ = fg;
            named_from_index(i)
                .and_then(named_rgb)
                .map(|(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
        }
    }
}

/// Standard xterm RGB for the 16 named ANSI colors.
fn named_rgb(named: NamedColor) -> Option<(u8, u8, u8)> {
    Some(match named {
        NamedColor::Black => (0, 0, 0),
        NamedColor::Red => (0xcd, 0x00, 0x00),
        NamedColor::Green => (0x00, 0xcd, 0x00),
        NamedColor::Yellow => (0xcd, 0xcd, 0x00),
        NamedColor::Blue => (0x00, 0x00, 0xee),
        NamedColor::Magenta => (0xcd, 0x00, 0xcd),
        NamedColor::Cyan => (0x00, 0xcd, 0xcd),
        NamedColor::White => (0xe5, 0xe5, 0xe5),
        NamedColor::BrightBlack => (0x7f, 0x7f, 0x7f),
        NamedColor::BrightRed => (0xff, 0x00, 0x00),
        NamedColor::BrightGreen => (0x00, 0xff, 0x00),
        NamedColor::BrightYellow => (0xff, 0xff, 0x00),
        NamedColor::BrightBlue => (0x5c, 0x5c, 0xff),
        NamedColor::BrightMagenta => (0xff, 0x00, 0xff),
        NamedColor::BrightCyan => (0x00, 0xff, 0xff),
        NamedColor::BrightWhite => (0xff, 0xff, 0xff),
        _ => return None,
    })
}

/// Map palette indices 0..=15 back to their named color.
fn named_from_index(i: u8) -> Option<NamedColor> {
    Some(match i {
        0 => NamedColor::Black,
        1 => NamedColor::Red,
        2 => NamedColor::Green,
        3 => NamedColor::Yellow,
        4 => NamedColor::Blue,
        5 => NamedColor::Magenta,
        6 => NamedColor::Cyan,
        7 => NamedColor::White,
        8 => NamedColor::BrightBlack,
        9 => NamedColor::BrightRed,
        10 => NamedColor::BrightGreen,
        11 => NamedColor::BrightYellow,
        12 => NamedColor::BrightBlue,
        13 => NamedColor::BrightMagenta,
        14 => NamedColor::BrightCyan,
        15 => NamedColor::BrightWhite,
        _ => return None,
    })
}

fn append_color(params: &mut Vec<String>, color: Color, fg: bool) {
    match color {
        Color::Named(NamedColor::Foreground) | Color::Named(NamedColor::Background) => {}
        Color::Named(named) => {
            if let Some(code) = named_sgr(named, fg) {
                params.push(code.to_string());
            }
        }
        Color::Spec(rgb) => {
            let lead = if fg { 38 } else { 48 };
            params.push(format!("{};2;{};{};{}", lead, rgb.r, rgb.g, rgb.b));
        }
        Color::Indexed(i) => {
            let lead = if fg { 38 } else { 48 };
            params.push(format!("{};5;{}", lead, i));
        }
    }
}

fn named_sgr(named: NamedColor, fg: bool) -> Option<u16> {
    let base = match named {
        NamedColor::Black => 0,
        NamedColor::Red => 1,
        NamedColor::Green => 2,
        NamedColor::Yellow => 3,
        NamedColor::Blue => 4,
        NamedColor::Magenta => 5,
        NamedColor::Cyan => 6,
        NamedColor::White => 7,
        NamedColor::BrightBlack => 8,
        NamedColor::BrightRed => 9,
        NamedColor::BrightGreen => 10,
        NamedColor::BrightYellow => 11,
        NamedColor::BrightBlue => 12,
        NamedColor::BrightMagenta => 13,
        NamedColor::BrightCyan => 14,
        NamedColor::BrightWhite => 15,
        _ => return None,
    };
    Some(if base < 8 {
        if fg { 30 + base } else { 40 + base }
    } else if fg {
        90 + (base - 8)
    } else {
        100 + (base - 8)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(rows: u16, cols: u16, data: &[u8]) -> TermState {
        let mut t = TermState::new(rows, cols);
        t.process(data);
        t
    }

    fn roundtrip(source: &TermState) -> TermState {
        let (rows, cols) = source.size();
        let mut dest = TermState::new(rows, cols);
        dest.process(&source.serialize_state().expect("state should not be empty"));
        dest
    }

    fn cursor_of(t: &TermState) -> (i32, usize) {
        let c = t.term.grid().cursor.point;
        (c.line.0, c.column.0)
    }

    /// Collect the receiver's scrollback history (negative lines) as text.
    fn history_lines(t: &TermState) -> Vec<String> {
        let grid = t.term.grid();
        let top = -(grid.history_size() as i32);
        let mut lines = Vec::new();
        for line in top..0 {
            let mut s = String::new();
            for col in 0..grid.columns() {
                let cell = &grid[Line(line)][Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                s.push(cell.c);
            }
            lines.push(s.trim_end().to_string());
        }
        lines
    }

    #[test]
    fn roundtrip_preserves_positioned_content() {
        let source = terminal(
            24,
            80,
            b"\x1b[2J\x1b[2;5HMARK_A\x1b[10;30HMARK_B\x1b[16;20H",
        );
        let dest = roundtrip(&source);
        assert_eq!(dest.contents_plain(), source.contents_plain());
        assert_eq!(cursor_of(&dest), cursor_of(&source));
    }

    #[test]
    fn roundtrip_preserves_visible_content_after_scrollback() {
        let mut source = terminal(24, 80, b"");
        for line in 0..80 {
            source.process(format!("SCROLL_{line}\r\n").as_bytes());
        }
        source.process(b"\x1b[2J\x1b[2;5HMARK_A\x1b[6;15HMARK_B\x1b[10;30HMARK_C\x1b[16;20H");
        let dest = roundtrip(&source);
        assert_eq!(dest.contents_plain(), source.contents_plain());
    }

    #[test]
    fn roundtrip_does_not_leak_inactive_alternate_screen() {
        let source = terminal(
            24,
            80,
            b"\x1b[?1049h\x1b[2J\x1b[3;10HALT_MARK\x1b[?1049l\x1b[2J\x1b[2;5HMAIN_MARK\x1b[8;20H",
        );
        let dest = roundtrip(&source);
        assert_eq!(dest.contents_plain(), source.contents_plain());
        assert!(!dest.alternate_screen());
        assert!(!String::from_utf8_lossy(&dest.contents_plain()).contains("ALT_MARK"));
    }

    #[test]
    fn roundtrip_preserves_active_alternate_screen() {
        let source = terminal(24, 80, b"\x1b[?1049h\x1b[2J\x1b[3;10HALT_MARK\x1b[8;20H");
        let dest = roundtrip(&source);
        assert!(dest.alternate_screen());
        assert!(String::from_utf8_lossy(&dest.contents_plain()).contains("ALT_MARK"));
    }

    #[test]
    fn roundtrip_after_resize_preserves_visible_state() {
        let mut source = terminal(
            30,
            80,
            b"\x1b[2J\x1b[3;10HSIZE_A\x1b[12;20HSIZE_B\x1b[20;40HSIZE_C\x1b[15;15H",
        );
        source.set_size(24, 80);
        let dest = roundtrip(&source);
        assert_eq!(dest.contents_plain(), source.contents_plain());
    }

    #[test]
    fn roundtrip_replays_scrollback_into_receiver_history() {
        // Fill well past the viewport so lines spill into scrollback.
        let mut source = terminal(24, 80, b"");
        for line in 0..80 {
            source.process(format!("SCROLL_{line}\r\n").as_bytes());
        }
        let dest = roundtrip(&source);

        // Visible screen still matches.
        assert_eq!(dest.contents_plain(), source.contents_plain());

        // And the scrollback history is reproduced on the receiver: early
        // lines that scrolled off the top are recoverable from its history.
        let dest_hist = history_lines(&dest);
        assert!(
            dest_hist.iter().any(|l| l == "SCROLL_0"),
            "receiver scrollback should contain the oldest line, got: {dest_hist:?}"
        );
        assert!(
            dest_hist.iter().any(|l| l == "SCROLL_40"),
            "receiver scrollback should contain a mid-history line, got: {dest_hist:?}"
        );
        // The source's full history should survive the roundtrip.
        assert_eq!(history_lines(&dest), history_lines(&source));
    }

    #[test]
    fn html_export_preserves_color() {
        // Red "hi" on default bg. Plain contents unaffected; SGR carried in state.
        let source = terminal(24, 80, b"\x1b[31mhi\x1b[0m");
        let state = source.serialize_state().expect("state");
        // Foreground red = SGR 31 should appear in the serialized stream.
        assert!(
            String::from_utf8_lossy(&state).contains("31"),
            "expected red SGR in serialized state"
        );
    }

    #[test]
    fn tracks_and_replays_window_title() {
        // OSC 2 sets the window title; it should be captured and replayed.
        let source = terminal(24, 80, b"\x1b]2;my-title\x07hello");
        assert_eq!(source.title().as_deref(), Some("my-title"));

        let dest = roundtrip(&source);
        assert_eq!(dest.title().as_deref(), Some("my-title"));
    }

    #[test]
    fn tracks_and_replays_osc7_cwd() {
        // OSC 7 reports the working directory as a file URI.
        let source = terminal(
            24,
            80,
            b"\x1b]7;file://localhost/home/user/proj\x1b\\prompt$ ",
        );
        assert_eq!(
            source.cwd_uri().as_deref(),
            Some("file://localhost/home/user/proj")
        );
        assert_eq!(source.cwd().as_deref(), Some("/home/user/proj"));

        // The serialized state carries the complete OSC 7 URI.
        let dest = roundtrip(&source);
        assert_eq!(
            dest.cwd_uri().as_deref(),
            Some("file://localhost/home/user/proj")
        );
        assert_eq!(dest.cwd().as_deref(), Some("/home/user/proj"));
    }

    #[test]
    fn osc7_percent_decodes_and_takes_latest() {
        let source = terminal(
            24,
            80,
            b"\x1b]7;file://localhost/tmp/a%20b\x07\x1b]7;file://localhost/tmp/final\x07",
        );
        assert_eq!(source.cwd().as_deref(), Some("/tmp/final"));

        let earlier = terminal(24, 80, b"\x1b]7;file://localhost/tmp/a%20b\x07");
        assert_eq!(earlier.cwd().as_deref(), Some("/tmp/a b"));
    }

    #[test]
    fn osc7_preserves_remote_host_without_exposing_local_path() {
        let source = terminal(24, 80, b"\x1b]7;file://remote.example/home/me\x07");
        assert_eq!(
            source.cwd_uri().as_deref(),
            Some("file://remote.example/home/me")
        );
        assert_eq!(source.cwd(), None);
    }

    #[test]
    fn roundtrip_preserves_input_modes_and_hidden_cursor() {
        // App-cursor, app-keypad, bracketed-paste, and a hidden cursor should
        // all survive a reattach roundtrip.
        let source = terminal(24, 80, b"content\x1b[?1h\x1b=\x1b[?2004h\x1b[?25l");
        let state = source.serialize_state().expect("state");
        let s = String::from_utf8_lossy(&state);
        assert!(s.contains("\x1b[?1h"), "app-cursor mode should be replayed");
        assert!(s.contains("\x1b="), "app-keypad mode should be replayed");
        assert!(
            s.contains("\x1b[?2004h"),
            "bracketed-paste should be replayed"
        );
        assert!(s.contains("\x1b[?25l"), "hidden cursor should be replayed");

        let dest = roundtrip(&source);
        use alacritty_terminal::term::TermMode;
        let mode = dest.term.mode();
        assert!(mode.contains(TermMode::APP_CURSOR));
        assert!(mode.contains(TermMode::APP_KEYPAD));
        assert!(mode.contains(TermMode::BRACKETED_PASTE));
        assert!(!mode.contains(TermMode::SHOW_CURSOR));
    }

    #[test]
    fn roundtrip_preserves_mouse_tracking_modes() {
        // A mouse-aware program (vim, fzf, less) inside the session enables
        // tracking. The client blanket-disables every mouse mode at attach to
        // scrub state a previous session left sticky, so the replay is the only
        // thing that can put it back — the inner program still thinks tracking
        // is on and will never re-send the enable. If this regresses, the mouse
        // (including the wheel) silently stops reaching the program after a
        // reattach.
        use alacritty_terminal::term::TermMode;
        let source = terminal(24, 80, b"content\x1b[?1002h\x1b[?1006h");
        let state = source.serialize_state().expect("state");
        let s = String::from_utf8_lossy(&state);
        assert!(
            s.contains("\x1b[?1002h"),
            "button-event tracking should be replayed, got: {s:?}"
        );
        assert!(
            s.contains("\x1b[?1006h"),
            "SGR mouse encoding should be replayed, got: {s:?}"
        );
        // The tracking mode must precede the encoding: some terminals reset the
        // encoding when the tracking mode changes.
        assert!(
            s.find("\x1b[?1002h") < s.find("\x1b[?1006h"),
            "tracking mode should be emitted before the encoding"
        );

        let dest = roundtrip(&source);
        let mode = dest.term.mode();
        assert!(
            mode.contains(TermMode::MOUSE_DRAG),
            "receiver should have button-event tracking on"
        );
        assert!(
            mode.contains(TermMode::SGR_MOUSE),
            "receiver should have SGR mouse encoding on"
        );

        // A session with no mouse activity must not gain tracking: emitting it
        // unconditionally would break wheel-scrolling in plain shells, which is
        // the exact symptom this fix is meant to cure.
        let plain = terminal(24, 80, b"just text");
        let plain_state = plain.serialize_state().expect("state");
        let p = String::from_utf8_lossy(&plain_state);
        assert!(
            !p.contains("\x1b[?1002h") && !p.contains("\x1b[?1006h"),
            "a mouseless session must not enable tracking, got: {p:?}"
        );
    }

    #[test]
    fn serialized_state_excludes_synchronized_output() {
        // DECSET 2026 (synchronized output) is a transient render hint and must
        // not be replayed, or a reattaching terminal would freeze its display.
        // vte buffers content until the sync ends (2026l), so close it here.
        let source = terminal(24, 80, b"\x1b[?2004h\x1b[?2026hhello\x1b[?2026l");
        let state = source.serialize_state().expect("state");
        assert!(
            state.windows(8).any(|w| w == b"\x1b[?2004h"),
            "bracketed-paste should be replayed"
        );
        assert!(
            !state.windows(8).any(|w| w == b"\x1b[?2026h"),
            "synchronized-output must not be replayed"
        );
        // The content written inside the sync should still be present.
        assert!(String::from_utf8_lossy(&source.contents_plain()).contains("hello"));
    }
}
