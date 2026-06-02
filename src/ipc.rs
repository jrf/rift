use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
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
    Print = 14,
    SshAuthSock = 15,
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
            14 => Some(Tag::Print),
            15 => Some(Tag::SshAuthSock),
            _ => None,
        }
    }
}

pub const HEADER_SIZE: usize = 5; // 1 byte tag + 4 bytes len

#[derive(Debug, Clone, Copy)]
pub struct Resize {
    pub rows: u16,
    pub cols: u16,
}

impl Resize {
    pub const WIRE_LEN: usize = 4;

    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut buf = [0u8; Self::WIRE_LEN];
        buf[0..2].copy_from_slice(&self.rows.to_le_bytes());
        buf[2..4].copy_from_slice(&self.cols.to_le_bytes());
        buf
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < Self::WIRE_LEN {
            return None;
        }
        Some(Resize {
            rows: u16::from_le_bytes([data[0], data[1]]),
            cols: u16::from_le_bytes([data[2], data[3]]),
        })
    }
}

#[derive(Debug, Clone, Default)]
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
        if data.len() < Self::HEADER_LEN + cmd_len + cwd_len {
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
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0 {
            return Resize {
                rows: ws.ws_row,
                cols: ws.ws_col,
            };
        }
    }
    Resize { rows: 24, cols: 80 }
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
        match unistd::write(&bfd, &data[offset..]) {
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
        let n = unistd::read(&bfd, &mut tmp)?;
        if n > 0 {
            self.buf.extend_from_slice(&tmp[..n]);
        }
        Ok(n)
    }

    /// Returns the next complete message or None.
    /// The returned slice borrows from the buffer; convert with `.to_vec()`
    /// if you need to release the borrow before the next iteration.
    pub fn next(&mut self) -> Option<(Tag, &[u8])> {
        let available = &self.buf[self.head..];
        if available.len() < HEADER_SIZE {
            return None;
        }

        let (tag_byte, len) = decode_header(available);
        let total = HEADER_SIZE + len as usize;
        if available.len() < total {
            return None;
        }

        let tag = Tag::from_u8(tag_byte);
        let start = self.head + HEADER_SIZE;
        let end = self.head + total;
        self.head += total;

        tag.map(|t| (t, &self.buf[start..end]))
    }
}

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

    let mut sb = SocketBuffer::new();
    let deadline = Instant::now() + Duration::from_millis(2000);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ProbeError::Timeout);
        }
        let timeout_ms = (remaining.as_millis() as u64).min(u16::MAX as u64) as u16;

        let bfd = unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) };
        let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
        let r = poll(&mut poll_fds, PollTimeout::from(timeout_ms))
            .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
        if r == 0 {
            return Err(ProbeError::Timeout);
        }

        let n = sb
            .read(fd.as_raw_fd())
            .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
        if n == 0 {
            return Err(ProbeError::Unexpected("connection closed".into()));
        }

        while let Some((tag, payload)) = sb.next() {
            if tag == Tag::Info {
                if let Some(info) = Info::decode(payload) {
                    return Ok(ProbeResult { fd, info });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// RiftCodec — tokio_util Encoder/Decoder for the wire protocol
// ---------------------------------------------------------------------------

/// Async codec for the rift wire protocol, used with `tokio_util::codec::Framed`.
/// Wire format per frame: `[1 byte tag][4 bytes len LE][payload]`. Unknown
/// tag bytes are silently skipped (matching `SocketBuffer::next` behavior).
pub struct RiftCodec;

impl Decoder for RiftCodec {
    type Item = (Tag, Bytes);
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < HEADER_SIZE {
            return Ok(None);
        }
        let len = u32::from_le_bytes([src[1], src[2], src[3], src[4]]) as usize;
        let total = HEADER_SIZE + len;
        if src.len() < total {
            src.reserve(total - src.len());
            return Ok(None);
        }
        let tag_byte = src[0];
        src.advance(HEADER_SIZE);
        let payload = src.split_to(len).freeze();
        Ok(Tag::from_u8(tag_byte).map(|t| (t, payload)))
    }
}

impl Encoder<(Tag, Bytes)> for RiftCodec {
    type Error = io::Error;

    fn encode(&mut self, item: (Tag, Bytes), dst: &mut BytesMut) -> Result<(), Self::Error> {
        let (tag, payload) = item;
        dst.reserve(HEADER_SIZE + payload.len());
        dst.put_u8(tag as u8);
        dst.put_u32_le(payload.len() as u32);
        dst.extend_from_slice(&payload);
        Ok(())
    }
}
