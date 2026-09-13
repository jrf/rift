use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd;
use tokio_util::codec::{Decoder, Encoder};

use crate::socket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    Input = 0,
    Output = 1,
    Resize = 2,
    Detach = 3,
    DetachAll = 4,
    Kill = 5,
    Info = 6,
    Init = 7,
    History = 8,
    Run = 9,
    TaskComplete = 10,
    Switch = 11,
    /// Subscribe a non-terminal client to live PTY output (`rift tail`).
    Tail = 12,
    Print = 14,
    SshAuthSock = 15,
    Rename = 16,
    Ack = 17,
    LabelGet = 18,
    LabelSet = 19,
    LabelClear = 20,
    LabelData = 21,
    EnvSet = 22,
    EnvGet = 23,
    EnvData = 24,
    /// Serialized terminal replay sent when a terminal reattaches.
    TerminalState = 25,
    /// Ask the interactive leader to report its current dimensions.
    ResizeRequest = 26,
    /// Result of a session rename; empty means success, UTF-8 text is an error.
    RenameResult = 27,
}

impl Tag {
    pub fn from_u8(v: u8) -> Option<Tag> {
        match v {
            0 => Some(Tag::Input),
            1 => Some(Tag::Output),
            2 => Some(Tag::Resize),
            3 => Some(Tag::Detach),
            4 => Some(Tag::DetachAll),
            5 => Some(Tag::Kill),
            6 => Some(Tag::Info),
            7 => Some(Tag::Init),
            8 => Some(Tag::History),
            9 => Some(Tag::Run),
            10 => Some(Tag::TaskComplete),
            11 => Some(Tag::Switch),
            12 => Some(Tag::Tail),
            14 => Some(Tag::Print),
            15 => Some(Tag::SshAuthSock),
            16 => Some(Tag::Rename),
            17 => Some(Tag::Ack),
            18 => Some(Tag::LabelGet),
            19 => Some(Tag::LabelSet),
            20 => Some(Tag::LabelClear),
            21 => Some(Tag::LabelData),
            22 => Some(Tag::EnvSet),
            23 => Some(Tag::EnvGet),
            24 => Some(Tag::EnvData),
            25 => Some(Tag::TerminalState),
            26 => Some(Tag::ResizeRequest),
            27 => Some(Tag::RenameResult),
            _ => None,
        }
    }
}

pub const HEADER_SIZE: usize = 5; // 1 byte tag + 4 bytes len
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub const REQUEST_ID_SIZE: usize = std::mem::size_of::<u64>();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryFormat {
    Plain,
    Vt,
    Html,
}

impl HistoryFormat {
    fn decode(payload: &[u8]) -> Option<Self> {
        match payload {
            [] | [0] => Some(Self::Plain),
            [1] => Some(Self::Vt),
            [2] => Some(Self::Html),
            _ => None,
        }
    }

    pub fn encode(self) -> [u8; 1] {
        [match self {
            Self::Plain => 0,
            Self::Vt => 1,
            Self::Html => 2,
        }]
    }
}

/// Validated client-to-daemon messages. This preserves the existing tag wire
/// format while moving direction and payload-shape semantics out of daemon
/// request handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientRequest {
    Input(Bytes),
    Resize(Resize),
    Detach,
    DetachAll,
    Kill,
    Info,
    AttachTerminal(Resize),
    History(HistoryFormat),
    Run(Bytes),
    Switch(String),
    SubscribeOutput,
    Print(Bytes),
    SshAuthSock(String),
    Rename(String),
    LabelGet,
    LabelSet(String),
    LabelClear,
    EnvSet(Option<String>),
    EnvGet,
}

impl ClientRequest {
    pub fn decode(tag: Tag, payload: Bytes) -> io::Result<Self> {
        fn require_empty(tag: Tag, payload: &Bytes) -> io::Result<()> {
            if payload.is_empty() {
                Ok(())
            } else {
                Err(invalid_payload(tag, "expected an empty payload"))
            }
        }

        fn utf8(tag: Tag, payload: &Bytes, allow_empty: bool) -> io::Result<String> {
            let value =
                std::str::from_utf8(payload).map_err(|_| invalid_payload(tag, "expected UTF-8"))?;
            if !allow_empty && value.is_empty() {
                return Err(invalid_payload(tag, "expected a non-empty payload"));
            }
            Ok(value.to_string())
        }

        let request = match tag {
            Tag::Input => Self::Input(payload),
            Tag::Resize => Self::Resize(
                Resize::decode(&payload).ok_or_else(|| invalid_payload(tag, "invalid resize"))?,
            ),
            Tag::Detach => {
                require_empty(tag, &payload)?;
                Self::Detach
            }
            Tag::DetachAll => {
                require_empty(tag, &payload)?;
                Self::DetachAll
            }
            Tag::Kill => {
                require_empty(tag, &payload)?;
                Self::Kill
            }
            Tag::Info => {
                require_empty(tag, &payload)?;
                Self::Info
            }
            Tag::Init => Self::AttachTerminal(
                Resize::decode(&payload).ok_or_else(|| invalid_payload(tag, "invalid resize"))?,
            ),
            Tag::History => Self::History(
                HistoryFormat::decode(&payload)
                    .ok_or_else(|| invalid_payload(tag, "invalid history format"))?,
            ),
            Tag::Run if !payload.is_empty() => Self::Run(payload),
            Tag::Run => return Err(invalid_payload(tag, "expected a non-empty payload")),
            Tag::Switch => Self::Switch(utf8(tag, &payload, false)?),
            Tag::Tail => {
                require_empty(tag, &payload)?;
                Self::SubscribeOutput
            }
            Tag::Print => Self::Print(payload),
            Tag::SshAuthSock => Self::SshAuthSock(utf8(tag, &payload, false)?),
            Tag::Rename => Self::Rename(utf8(tag, &payload, false)?),
            Tag::LabelGet => {
                require_empty(tag, &payload)?;
                Self::LabelGet
            }
            Tag::LabelSet => Self::LabelSet(utf8(tag, &payload, true)?),
            Tag::LabelClear => {
                require_empty(tag, &payload)?;
                Self::LabelClear
            }
            Tag::EnvSet => Self::EnvSet(if payload.is_empty() {
                None
            } else {
                Some(utf8(tag, &payload, false)?)
            }),
            Tag::EnvGet => {
                require_empty(tag, &payload)?;
                Self::EnvGet
            }
            Tag::Output
            | Tag::TaskComplete
            | Tag::Ack
            | Tag::LabelData
            | Tag::EnvData
            | Tag::TerminalState
            | Tag::ResizeRequest
            | Tag::RenameResult => {
                return Err(invalid_payload(tag, "tag is not a client request"));
            }
        };
        Ok(request)
    }
}

/// Validated daemon-to-client messages. Encoding uses dedicated directional
/// tags for terminal replay and size requests; decoding also accepts the legacy
/// overloaded `Init` and empty `Resize` forms for wire compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonEvent {
    Output(Bytes),
    TerminalState(Bytes),
    ResizeRequest,
    Detach,
    Info(Info),
    History(Bytes),
    TaskComplete { request_id: u64, exit_code: u8 },
    Switch { name: String, cwd: Option<String> },
    Ack,
    LabelData(String),
    EnvData(String),
    RenameResult(Result<(), String>),
}

impl DaemonEvent {
    pub fn encode(self) -> (Tag, Bytes) {
        match self {
            Self::Output(payload) => (Tag::Output, payload),
            // Keep emitting the legacy tags until protocol capability
            // negotiation exists; decoding accepts both legacy and dedicated
            // forms so the semantic API itself is no longer overloaded.
            Self::TerminalState(payload) => (Tag::Init, payload),
            Self::ResizeRequest => (Tag::Resize, Bytes::new()),
            Self::Detach => (Tag::Detach, Bytes::new()),
            Self::Info(info) => (Tag::Info, Bytes::from(info.encode())),
            Self::History(payload) => (Tag::History, payload),
            Self::TaskComplete {
                request_id,
                exit_code,
            } => (
                Tag::TaskComplete,
                encode_task_complete(request_id, exit_code),
            ),
            Self::Switch { name, cwd } => (Tag::Switch, encode_switch(&name, cwd.as_deref())),
            Self::Ack => (Tag::Ack, Bytes::new()),
            Self::LabelData(labels) => (Tag::LabelData, Bytes::from(labels.into_bytes())),
            Self::EnvData(environment) => (Tag::EnvData, Bytes::from(environment.into_bytes())),
            Self::RenameResult(result) => (
                Tag::RenameResult,
                Bytes::from(result.err().unwrap_or_default().into_bytes()),
            ),
        }
    }

    pub fn decode(tag: Tag, payload: Bytes) -> io::Result<Self> {
        fn require_empty(tag: Tag, payload: &Bytes) -> io::Result<()> {
            if payload.is_empty() {
                Ok(())
            } else {
                Err(invalid_payload(tag, "expected an empty payload"))
            }
        }

        fn utf8(tag: Tag, payload: &Bytes) -> io::Result<String> {
            std::str::from_utf8(payload)
                .map(str::to_string)
                .map_err(|_| invalid_payload(tag, "expected UTF-8"))
        }

        let event = match tag {
            Tag::Output => Self::Output(payload),
            Tag::TerminalState | Tag::Init => Self::TerminalState(payload),
            Tag::ResizeRequest => {
                require_empty(tag, &payload)?;
                Self::ResizeRequest
            }
            Tag::Resize if payload.is_empty() => Self::ResizeRequest,
            Tag::Resize => return Err(invalid_payload(tag, "expected an empty payload")),
            Tag::Detach => {
                require_empty(tag, &payload)?;
                Self::Detach
            }
            Tag::Info => Self::Info(
                Info::decode(&payload).ok_or_else(|| invalid_payload(tag, "invalid info"))?,
            ),
            Tag::History => Self::History(payload),
            Tag::TaskComplete => {
                let (request_id, exit_code) = decode_task_complete(&payload)
                    .ok_or_else(|| invalid_payload(tag, "invalid task completion"))?;
                Self::TaskComplete {
                    request_id,
                    exit_code,
                }
            }
            Tag::Switch => {
                let (name, cwd) = decode_switch(&payload)
                    .ok_or_else(|| invalid_payload(tag, "invalid switch target"))?;
                Self::Switch { name, cwd }
            }
            Tag::Ack => {
                require_empty(tag, &payload)?;
                Self::Ack
            }
            Tag::LabelData => Self::LabelData(utf8(tag, &payload)?),
            Tag::EnvData => Self::EnvData(utf8(tag, &payload)?),
            Tag::RenameResult => {
                let message = utf8(tag, &payload)?;
                Self::RenameResult(if message.is_empty() {
                    Ok(())
                } else {
                    Err(message)
                })
            }
            Tag::Input
            | Tag::DetachAll
            | Tag::Kill
            | Tag::Run
            | Tag::Tail
            | Tag::Print
            | Tag::SshAuthSock
            | Tag::Rename
            | Tag::LabelGet
            | Tag::LabelSet
            | Tag::LabelClear
            | Tag::EnvSet
            | Tag::EnvGet => {
                return Err(invalid_payload(tag, "tag is not a daemon event"));
            }
        };
        Ok(event)
    }
}

fn invalid_payload(tag: Tag, message: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid {:?} frame: {}", tag, message),
    )
}

pub fn encode_switch(name: &str, cwd: Option<&str>) -> Bytes {
    let mut payload = Vec::with_capacity(name.len() + cwd.map_or(0, |cwd| cwd.len() + 1));
    payload.extend_from_slice(name.as_bytes());
    if let Some(cwd) = cwd {
        payload.push(b'\n');
        payload.extend_from_slice(cwd.as_bytes());
    }
    Bytes::from(payload)
}

pub fn decode_switch(payload: &[u8]) -> Option<(String, Option<String>)> {
    let text = std::str::from_utf8(payload).ok()?;
    let (name, cwd) = match text.split_once('\n') {
        Some((name, cwd)) => (name, (!cwd.is_empty()).then(|| cwd.to_string())),
        None => (text, None),
    };
    (!name.is_empty()).then(|| (name.to_string(), cwd))
}

pub fn encode_task_complete(request_id: u64, exit_code: u8) -> Bytes {
    let mut payload = Vec::with_capacity(REQUEST_ID_SIZE + 1);
    payload.extend_from_slice(&request_id.to_le_bytes());
    payload.push(exit_code);
    Bytes::from(payload)
}

pub fn decode_task_complete(payload: &[u8]) -> Option<(u64, u8)> {
    if payload.len() != REQUEST_ID_SIZE + 1 {
        return None;
    }
    let request_id = u64::from_le_bytes(payload[..REQUEST_ID_SIZE].try_into().ok()?);
    Some((request_id, payload[REQUEST_ID_SIZE]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resize {
    pub rows: u16,
    pub cols: u16,
    /// Terminal width in pixels (ws_xpixel). 0 when the terminal doesn't report it.
    pub xpixel: u16,
    /// Terminal height in pixels (ws_ypixel). 0 when the terminal doesn't report it.
    pub ypixel: u16,
}

impl Resize {
    /// Full wire length. Older peers emit only the first 4 bytes (rows + cols);
    /// [`Resize::decode`] tolerates that and reports 0 pixels in that case.
    pub const WIRE_LEN: usize = 8;
    const MIN_WIRE_LEN: usize = 4;

    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut buf = [0u8; Self::WIRE_LEN];
        buf[0..2].copy_from_slice(&self.rows.to_le_bytes());
        buf[2..4].copy_from_slice(&self.cols.to_le_bytes());
        buf[4..6].copy_from_slice(&self.xpixel.to_le_bytes());
        buf[6..8].copy_from_slice(&self.ypixel.to_le_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if !matches!(data.len(), Self::MIN_WIRE_LEN | Self::WIRE_LEN) {
            return None;
        }
        // Pixel dimensions were added later; the legacy four-byte payload just
        // means the peer didn't send them, so default those fields to 0.
        let (xpixel, ypixel) = if data.len() == Self::WIRE_LEN {
            (
                u16::from_le_bytes([data[4], data[5]]),
                u16::from_le_bytes([data[6], data[7]]),
            )
        } else {
            (0, 0)
        };
        Some(Resize {
            rows: u16::from_le_bytes([data[0], data[1]]),
            cols: u16::from_le_bytes([data[2], data[3]]),
            xpixel,
            ypixel,
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Info {
    pub clients_len: usize,
    pub pid: i32,
    pub created_at: u64,
    pub task_ended_at: u64,
    pub task_exit_code: u8,
    pub cmd: Vec<u8>,
    pub cwd: Vec<u8>,
}

impl Info {
    // 8 (clients_len) + 4 (pid) + 8 (created_at) + 8 (task_ended_at)
    // + 1 (task_exit_code) + 2 (cmd_len) + 2 (cwd_len) = 33
    const HEADER_LEN: usize = 33;

    pub fn encode(&self) -> Vec<u8> {
        let cmd_len = self.cmd.len().min(u16::MAX as usize);
        let cwd_len = self.cwd.len().min(u16::MAX as usize);
        let mut buf = Vec::with_capacity(Self::HEADER_LEN + cmd_len + cwd_len);
        buf.extend_from_slice(&(self.clients_len as u64).to_le_bytes());
        buf.extend_from_slice(&self.pid.to_le_bytes());
        buf.extend_from_slice(&self.created_at.to_le_bytes());
        buf.extend_from_slice(&self.task_ended_at.to_le_bytes());
        buf.push(self.task_exit_code);
        buf.extend_from_slice(&(cmd_len as u16).to_le_bytes());
        buf.extend_from_slice(&(cwd_len as u16).to_le_bytes());
        buf.extend_from_slice(&self.cmd[..cmd_len]);
        buf.extend_from_slice(&self.cwd[..cwd_len]);
        buf
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < Self::HEADER_LEN {
            return None;
        }
        let clients_len = u64::from_le_bytes(data[0..8].try_into().ok()?) as usize;
        let pid = i32::from_le_bytes(data[8..12].try_into().ok()?);
        let created_at = u64::from_le_bytes(data[12..20].try_into().ok()?);
        let task_ended_at = u64::from_le_bytes(data[20..28].try_into().ok()?);
        let task_exit_code = data[28];
        let cmd_len = u16::from_le_bytes(data[29..31].try_into().ok()?) as usize;
        let cwd_len = u16::from_le_bytes(data[31..33].try_into().ok()?) as usize;
        if data.len() != Self::HEADER_LEN + cmd_len + cwd_len {
            return None;
        }
        let cmd_start = Self::HEADER_LEN;
        let cwd_start = cmd_start + cmd_len;
        Some(Info {
            clients_len,
            pid,
            created_at,
            task_ended_at,
            task_exit_code,
            cmd: data[cmd_start..cwd_start].to_vec(),
            cwd: data[cwd_start..cwd_start + cwd_len].to_vec(),
        })
    }
}

pub fn get_terminal_size(fd: RawFd) -> Resize {
    for candidate in [
        fd,
        libc::STDOUT_FILENO,
        libc::STDIN_FILENO,
        libc::STDERR_FILENO,
    ] {
        if let Some(size) = terminal_size(candidate) {
            return size;
        }
    }

    let tty = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if tty >= 0 {
        let size = terminal_size(tty);
        unsafe {
            libc::close(tty);
        }
        if let Some(size) = size {
            return size;
        }
    }

    Resize {
        rows: 24,
        cols: 120,
        xpixel: 0,
        ypixel: 0,
    }
}

fn terminal_size(fd: RawFd) -> Option<Resize> {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        (libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0)
            .then_some(Resize {
                rows: ws.ws_row,
                cols: ws.ws_col,
                xpixel: ws.ws_xpixel,
                ypixel: ws.ws_ypixel,
            })
    }
}

pub fn encode_header(tag: Tag, len: u32) -> [u8; HEADER_SIZE] {
    let mut buf = [0u8; HEADER_SIZE];
    buf[0] = tag as u8;
    buf[1..5].copy_from_slice(&len.to_le_bytes());
    buf
}

fn decode_header(data: &[u8]) -> (u8, u32) {
    let tag = data[0];
    let len = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
    (tag, len)
}

pub fn send(fd: RawFd, tag: Tag, data: &[u8]) -> io::Result<()> {
    if data.len() > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame exceeds maximum size",
        ));
    }
    let header = encode_header(tag, data.len() as u32);
    let mut msg = Vec::with_capacity(HEADER_SIZE + data.len());
    msg.extend_from_slice(&header);
    msg.extend_from_slice(data);
    write_all(fd, &msg)
}

pub fn write_all(fd: RawFd, data: &[u8]) -> io::Result<()> {
    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut offset = 0;
    while offset < data.len() {
        match unistd::write(bfd, &data[offset..]) {
            Ok(n) => {
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
                }
                offset += n;
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(e) => return Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
    Ok(())
}

pub struct SocketBuffer {
    buf: Vec<u8>,
    head: usize,
}

impl SocketBuffer {
    pub fn new() -> Self {
        SocketBuffer {
            buf: Vec::with_capacity(4096),
            head: 0,
        }
    }

    /// Reads from fd into buffer. Returns number of bytes read.
    pub fn read(&mut self, fd: RawFd) -> nix::Result<usize> {
        if self.head > 0 {
            let remaining = self.buf.len() - self.head;
            if remaining > 0 {
                self.buf.copy_within(self.head.., 0);
                self.buf.truncate(remaining);
            } else {
                self.buf.clear();
            }
            self.head = 0;
        }

        let mut tmp = [0u8; 4096];
        let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
        let n = unistd::read(bfd, &mut tmp)?;
        if n > 0 {
            self.buf.extend_from_slice(&tmp[..n]);
        }
        Ok(n)
    }

    /// Returns the next complete message, `None` when more bytes are needed,
    /// or `InvalidData` for malformed input. The returned slice borrows from
    /// the buffer; convert with `.to_vec()` before reading into it again.
    pub fn next(&mut self) -> io::Result<Option<(Tag, &[u8])>> {
        let available = &self.buf[self.head..];
        if available.len() < HEADER_SIZE {
            return Ok(None);
        }

        let (tag_byte, len) = decode_header(available);
        let len = len as usize;
        if len > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds maximum size",
            ));
        }
        let tag = Tag::from_u8(tag_byte).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown protocol tag: {tag_byte}"),
            )
        })?;
        let total = HEADER_SIZE + len;
        if available.len() < total {
            return Ok(None);
        }

        let start = self.head + HEADER_SIZE;
        let end = self.head + total;
        self.head += total;

        Ok(Some((tag, &self.buf[start..end])))
    }

    /// Decode the next complete frame as a validated daemon event.
    pub fn next_daemon_event(&mut self) -> io::Result<Option<DaemonEvent>> {
        let Some((tag, payload)) = self.next()? else {
            return Ok(None);
        };
        DaemonEvent::decode(tag, Bytes::copy_from_slice(payload)).map(Some)
    }
}

#[derive(Debug)]
pub enum ProbeError {
    Timeout,
    ConnectionRefused,
    Unexpected(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Timeout => write!(f, "timeout"),
            ProbeError::ConnectionRefused => write!(f, "connection refused"),
            ProbeError::Unexpected(msg) => write!(f, "{}", msg),
        }
    }
}

pub struct ProbeResult {
    pub fd: OwnedFd,
    pub info: Info,
    pub labels: Option<Vec<u8>>,
}

pub fn probe_session(socket_path: &str) -> Result<ProbeResult, ProbeError> {
    let fd = socket::session_connect(socket_path).map_err(|e| {
        if e.kind() == io::ErrorKind::ConnectionRefused {
            ProbeError::ConnectionRefused
        } else {
            ProbeError::Unexpected(format!("{}", e))
        }
    })?;

    send(fd.as_raw_fd(), Tag::Info, &[]).map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
    send(fd.as_raw_fd(), Tag::LabelGet, &[])
        .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;

    let mut sb = SocketBuffer::new();
    let deadline = Instant::now() + Duration::from_millis(2000);
    let mut info = None;
    let mut labels = None;
    let mut labels_deadline = None;

    loop {
        if labels.is_some()
            && let Some(info) = info
        {
            return Ok(ProbeResult { fd, info, labels });
        }

        let active_deadline = labels_deadline.unwrap_or(deadline);
        let remaining = active_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return match info {
                Some(info) => Ok(ProbeResult { fd, info, labels }),
                None => Err(ProbeError::Timeout),
            };
        }
        let timeout_ms = (remaining.as_millis() as u64).min(u16::MAX as u64) as u16;

        let bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
        let r = poll(&mut poll_fds, PollTimeout::from(timeout_ms))
            .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
        if r == 0 {
            return match info {
                Some(info) => Ok(ProbeResult { fd, info, labels }),
                None => Err(ProbeError::Timeout),
            };
        }

        let n = sb
            .read(fd.as_raw_fd())
            .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
        if n == 0 {
            return match info {
                Some(info) => Ok(ProbeResult { fd, info, labels }),
                None => Err(ProbeError::Unexpected("connection closed".into())),
            };
        }

        while let Some(event) = sb
            .next_daemon_event()
            .map_err(|error| ProbeError::Unexpected(error.to_string()))?
        {
            match event {
                DaemonEvent::Info(value) => {
                    info = Some(value);
                    if labels_deadline.is_none() {
                        labels_deadline = Some(Instant::now() + Duration::from_millis(50));
                    }
                }
                DaemonEvent::LabelData(value) => labels = Some(value.into_bytes()),
                _ => {}
            }
        }
    }
}

pub fn request_response(
    socket_path: &str,
    request_tag: Tag,
    payload: &[u8],
    response_tag: Tag,
) -> Result<Vec<u8>, ProbeError> {
    let fd = socket::session_connect(socket_path).map_err(|error| {
        if error.kind() == io::ErrorKind::ConnectionRefused {
            ProbeError::ConnectionRefused
        } else {
            ProbeError::Unexpected(error.to_string())
        }
    })?;
    send(fd.as_raw_fd(), request_tag, payload)
        .map_err(|error| ProbeError::Unexpected(error.to_string()))?;

    let mut buffer = SocketBuffer::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProbeError::Timeout);
        }
        let timeout_ms = (remaining.as_millis() as u64).min(u16::MAX as u64) as u16;
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        let ready = poll(&mut poll_fds, PollTimeout::from(timeout_ms))
            .map_err(|error| ProbeError::Unexpected(error.to_string()))?;
        if ready == 0 {
            return Err(ProbeError::Timeout);
        }
        let read = buffer
            .read(fd.as_raw_fd())
            .map_err(|error| ProbeError::Unexpected(error.to_string()))?;
        if read == 0 {
            return Err(ProbeError::Unexpected("connection closed".into()));
        }
        while let Some((tag, response)) = buffer
            .next()
            .map_err(|error| ProbeError::Unexpected(error.to_string()))?
        {
            DaemonEvent::decode(tag, Bytes::copy_from_slice(response))
                .map_err(|error| ProbeError::Unexpected(error.to_string()))?;
            if tag == response_tag {
                return Ok(response.to_vec());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RiftCodec — tokio_util Encoder/Decoder for the wire protocol
// ---------------------------------------------------------------------------

/// Async codec for the rift wire protocol, used with `tokio_util::codec::Framed`.
/// Wire format per frame: `[1 byte tag][4 bytes len LE][payload]`. Unknown tags
/// and oversized frames are rejected as malformed input.
pub struct RiftCodec;

impl Decoder for RiftCodec {
    type Item = (Tag, Bytes);
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < HEADER_SIZE {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[1], src[2], src[3], src[4]]) as usize;
        if len > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds maximum size",
            ));
        }
        let total = HEADER_SIZE + len;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        let tag_byte = src[0];
        let tag = Tag::from_u8(tag_byte).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown protocol tag: {tag_byte}"),
            )
        })?;
        src.advance(HEADER_SIZE);
        let payload = src.split_to(len).freeze();
        Ok(Some((tag, payload)))
    }
}

impl Encoder<(Tag, Bytes)> for RiftCodec {
    type Error = io::Error;

    fn encode(&mut self, item: (Tag, Bytes), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let (tag, payload) = item;
        if payload.len() > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame exceeds maximum size",
            ));
        }
        dst.reserve(HEADER_SIZE + payload.len());
        dst.put_u8(tag as u8);
        dst.put_u32_le(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_roundtrips_pixel_dimensions() {
        let r = Resize {
            rows: 40,
            cols: 120,
            xpixel: 1920,
            ypixel: 1080,
        };
        let decoded = Resize::decode(&r.encode()).expect("valid resize payload");
        assert_eq!(decoded.rows, 40);
        assert_eq!(decoded.cols, 120);
        assert_eq!(decoded.xpixel, 1920);
        assert_eq!(decoded.ypixel, 1080);
    }

    #[test]
    fn resize_decodes_legacy_4byte_payload_with_zero_pixels() {
        // A pre-pixel peer sends only rows + cols; pixel fields default to 0.
        let mut legacy = [0u8; 4];
        legacy[0..2].copy_from_slice(&40u16.to_le_bytes());
        legacy[2..4].copy_from_slice(&120u16.to_le_bytes());
        let decoded = Resize::decode(&legacy).expect("legacy resize payload");
        assert_eq!(decoded.rows, 40);
        assert_eq!(decoded.cols, 120);
        assert_eq!(decoded.xpixel, 0);
        assert_eq!(decoded.ypixel, 0);
    }

    #[test]
    fn protocol_tag_values_and_info_header_are_frozen() {
        let tags = [
            (Tag::Input, 0),
            (Tag::Output, 1),
            (Tag::Resize, 2),
            (Tag::Detach, 3),
            (Tag::DetachAll, 4),
            (Tag::Kill, 5),
            (Tag::Info, 6),
            (Tag::Init, 7),
            (Tag::History, 8),
            (Tag::Run, 9),
            (Tag::TaskComplete, 10),
            (Tag::Switch, 11),
            (Tag::Tail, 12),
            (Tag::Print, 14),
            (Tag::SshAuthSock, 15),
            (Tag::Rename, 16),
            (Tag::Ack, 17),
            (Tag::LabelGet, 18),
            (Tag::LabelSet, 19),
            (Tag::LabelClear, 20),
            (Tag::LabelData, 21),
            (Tag::EnvSet, 22),
            (Tag::EnvGet, 23),
            (Tag::EnvData, 24),
            (Tag::TerminalState, 25),
            (Tag::ResizeRequest, 26),
            (Tag::RenameResult, 27),
        ];
        for (tag, expected) in tags {
            assert_eq!(tag as u8, expected);
            assert_eq!(Tag::from_u8(expected), Some(tag));
        }
        assert_eq!(Info::default().encode().len(), Info::HEADER_LEN);
    }

    #[test]
    fn probe_keeps_old_daemons_without_label_support_listable() {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let socket_path = std::env::temp_dir().join(format!(
            "rift-old-daemon-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let listener = UnixListener::bind(&socket_path).expect("bind test socket");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept probe");
            let mut tags = Vec::new();
            for _ in 0..2 {
                let mut header = [0; HEADER_SIZE];
                stream.read_exact(&mut header).expect("read request header");
                let length = u32::from_le_bytes(header[1..].try_into().unwrap()) as usize;
                let mut payload = vec![0; length];
                stream.read_exact(&mut payload).expect("read request body");
                tags.push(header[0]);
            }
            let payload = Info {
                pid: 123,
                ..Info::default()
            }
            .encode();
            stream
                .write_all(&encode_header(Tag::Info, payload.len() as u32))
                .expect("write response header");
            stream.write_all(&payload).expect("write response body");
            std::thread::sleep(Duration::from_millis(100));
            tags
        });

        let result = probe_session(socket_path.to_str().unwrap()).expect("probe old daemon");
        assert_eq!(result.info.pid, 123);
        assert_eq!(result.labels, None);
        assert_eq!(
            server.join().expect("join server"),
            vec![Tag::Info as u8, Tag::LabelGet as u8]
        );
        std::fs::remove_file(socket_path).expect("remove test socket");
    }

    #[test]
    fn info_round_trips() {
        let info = Info {
            clients_len: 3,
            pid: 1234,
            created_at: 10,
            task_ended_at: 20,
            task_exit_code: 7,
            cmd: b"cargo test".to_vec(),
            cwd: b"/tmp/project".to_vec(),
        };
        let decoded = Info::decode(&info.encode()).expect("valid info payload");
        assert_eq!(decoded.clients_len, info.clients_len);
        assert_eq!(decoded.pid, info.pid);
        assert_eq!(decoded.created_at, info.created_at);
        assert_eq!(decoded.task_ended_at, info.task_ended_at);
        assert_eq!(decoded.task_exit_code, info.task_exit_code);
        assert_eq!(decoded.cmd, info.cmd);
        assert_eq!(decoded.cwd, info.cwd);
    }

    #[test]
    fn task_completion_round_trips() {
        let completion = encode_task_complete(99, 2);
        assert_eq!(decode_task_complete(&completion), Some((99, 2)));
    }

    #[test]
    fn client_request_validates_payloads_and_direction() {
        let resize = Resize {
            rows: 40,
            cols: 120,
            xpixel: 1920,
            ypixel: 1080,
        };
        assert_eq!(
            ClientRequest::decode(Tag::Init, Bytes::copy_from_slice(&resize.encode())).unwrap(),
            ClientRequest::AttachTerminal(resize)
        );
        assert_eq!(
            ClientRequest::decode(Tag::Resize, Bytes::from_static(&[40, 0, 120, 0])).unwrap(),
            ClientRequest::Resize(Resize {
                rows: 40,
                cols: 120,
                xpixel: 0,
                ypixel: 0,
            })
        );
        for malformed in [
            Bytes::new(),
            Bytes::from_static(&[1; 5]),
            Bytes::from_static(&[1; 9]),
        ] {
            assert!(ClientRequest::decode(Tag::Resize, malformed).is_err());
        }
        assert_eq!(
            ClientRequest::decode(Tag::History, Bytes::new()).unwrap(),
            ClientRequest::History(HistoryFormat::Plain)
        );
        assert_eq!(
            ClientRequest::decode(Tag::History, Bytes::from_static(&[2])).unwrap(),
            ClientRequest::History(HistoryFormat::Html)
        );
        assert!(ClientRequest::decode(Tag::History, Bytes::from_static(&[3])).is_err());
        assert!(ClientRequest::decode(Tag::Detach, Bytes::from_static(b"unexpected")).is_err());
        assert!(ClientRequest::decode(Tag::Run, Bytes::new()).is_err());
        assert!(ClientRequest::decode(Tag::Rename, Bytes::new()).is_err());
        assert!(ClientRequest::decode(Tag::Rename, Bytes::from_static(&[0xff])).is_err());
        assert!(ClientRequest::decode(Tag::Output, Bytes::new()).is_err());
    }

    #[test]
    fn daemon_events_validate_direction_payloads_and_legacy_forms() {
        let resize_request = DaemonEvent::ResizeRequest;
        assert_eq!(resize_request.clone().encode(), (Tag::Resize, Bytes::new()));
        assert_eq!(
            DaemonEvent::decode(Tag::Resize, Bytes::new()).unwrap(),
            resize_request
        );
        assert_eq!(
            DaemonEvent::decode(Tag::ResizeRequest, Bytes::new()).unwrap(),
            resize_request
        );
        assert!(DaemonEvent::decode(Tag::Resize, Bytes::from_static(&[1])).is_err());

        let replay = Bytes::from_static(b"terminal state");
        assert_eq!(
            DaemonEvent::TerminalState(replay.clone()).encode(),
            (Tag::Init, replay.clone())
        );
        assert_eq!(
            DaemonEvent::decode(Tag::Init, replay.clone()).unwrap(),
            DaemonEvent::TerminalState(replay.clone())
        );
        assert_eq!(
            DaemonEvent::decode(Tag::TerminalState, replay.clone()).unwrap(),
            DaemonEvent::TerminalState(replay)
        );

        assert_eq!(
            DaemonEvent::RenameResult(Ok(())).encode(),
            (Tag::RenameResult, Bytes::new())
        );
        assert_eq!(
            DaemonEvent::decode(Tag::RenameResult, Bytes::from_static(b"occupied")).unwrap(),
            DaemonEvent::RenameResult(Err("occupied".to_string()))
        );
        assert!(DaemonEvent::decode(Tag::Ack, Bytes::from_static(b"unexpected")).is_err());
        assert!(DaemonEvent::decode(Tag::Input, Bytes::new()).is_err());
    }

    #[test]
    fn switch_payload_round_trips() {
        assert_eq!(
            decode_switch(&encode_switch("target", Some("/tmp/work"))),
            Some(("target".to_string(), Some("/tmp/work".to_string())))
        );
        assert_eq!(
            decode_switch(&encode_switch("target", None)),
            Some(("target".to_string(), None))
        );
        assert_eq!(decode_switch(b"\n/tmp"), None);
        assert_eq!(decode_switch(&[0xff]), None);
    }

    #[test]
    fn socket_buffer_rejects_unknown_and_oversized_frames() {
        let mut unknown = SocketBuffer {
            buf: {
                let mut bytes = Vec::new();
                bytes.push(13);
                bytes.extend_from_slice(&0u32.to_le_bytes());
                bytes.extend_from_slice(&encode_header(Tag::Input, 4));
                bytes.extend_from_slice(b"next");
                bytes
            },
            head: 0,
        };
        let error = unknown.next().expect_err("unknown tag must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);

        let mut oversized = SocketBuffer {
            buf: {
                let mut bytes = Vec::new();
                bytes.push(Tag::Input as u8);
                bytes.extend_from_slice(&((MAX_FRAME_SIZE as u32) + 1).to_le_bytes());
                bytes
            },
            head: 0,
        };
        let error = oversized
            .next()
            .expect_err("oversized frame must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn socket_buffer_distinguishes_partial_and_complete_frames() {
        let mut buffer = SocketBuffer {
            buf: encode_header(Tag::Output, 5).to_vec(),
            head: 0,
        };
        assert!(buffer.next().unwrap().is_none());
        buffer.buf.extend_from_slice(b"hello");
        assert_eq!(
            buffer.next().unwrap(),
            Some((Tag::Output, b"hello".as_slice()))
        );
        assert!(buffer.next().unwrap().is_none());
    }

    #[test]
    fn codec_rejects_oversized_frame_before_reserving_payload() {
        let mut encoded = BytesMut::new();
        encoded.put_u8(Tag::Input as u8);
        encoded.put_u32_le((MAX_FRAME_SIZE as u32) + 1);

        let error = RiftCodec
            .decode(&mut encoded)
            .expect_err("oversized frame must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn codec_rejects_unknown_tag_even_when_followed_by_valid_frame() {
        let mut encoded = BytesMut::new();
        encoded.put_u8(13);
        encoded.put_u32_le(0);
        RiftCodec
            .encode((Tag::Input, Bytes::from_static(b"next")), &mut encoded)
            .unwrap();

        let error = RiftCodec
            .decode(&mut encoded)
            .expect_err("unknown tag must be rejected");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn codec_round_trips_frame() {
        let mut encoded = BytesMut::new();
        RiftCodec
            .encode((Tag::Output, Bytes::from_static(b"hello")), &mut encoded)
            .expect("encode");
        assert_eq!(
            RiftCodec.decode(&mut encoded).expect("decode"),
            Some((Tag::Output, Bytes::from_static(b"hello")))
        );
    }
}
