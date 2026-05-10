use std::io;
use std::os::unix::io::{BorrowedFd, RawFd};

use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use nix::unistd;

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
    Ack = 10,
    Switch = 11,
    Write = 12,
    TaskComplete = 13,
    Print = 14,
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
            10 => Some(Tag::Ack),
            11 => Some(Tag::Switch),
            12 => Some(Tag::Write),
            13 => Some(Tag::TaskComplete),
            14 => Some(Tag::Print),
            _ => None,
        }
    }
}

pub const HEADER_SIZE: usize = 5; // 1 byte tag + 4 bytes len

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct Resize {
    pub rows: u16,
    pub cols: u16,
}

pub const MAX_CMD_LEN: usize = 256;
pub const MAX_CWD_LEN: usize = 256;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct Info {
    pub clients_len: usize,
    pub pid: i32,
    pub cmd_len: u16,
    pub cwd_len: u16,
    pub cmd: [u8; MAX_CMD_LEN],
    pub cwd: [u8; MAX_CWD_LEN],
    pub created_at: u64,
    pub task_ended_at: u64,
    pub task_exit_code: u8,
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

fn encode_header(tag: Tag, len: u32) -> [u8; HEADER_SIZE] {
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
    write_all(fd, &header)?;
    if !data.is_empty() {
        write_all(fd, data)?;
    }
    Ok(())
}

fn write_all(fd: RawFd, data: &[u8]) -> io::Result<()> {
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
    pub fn next(&mut self) -> Option<(Tag, Vec<u8>)> {
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
        let payload = available[HEADER_SIZE..total].to_vec();
        self.head += total;

        tag.map(|t| (t, payload))
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
    pub fd: RawFd,
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

    send(fd, Tag::Info, &[]).map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;

    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut poll_fds = [PollFd::new(bfd, PollFlags::POLLIN)];
    let poll_result = poll(&mut poll_fds, PollTimeout::from(1000u16))
        .map_err(|e| ProbeError::Unexpected(format!("{}", e)))?;
    if poll_result == 0 {
        let _ = unistd::close(fd);
        return Err(ProbeError::Timeout);
    }

    let mut sb = SocketBuffer::new();
    let n = sb.read(fd).map_err(|e| {
        let _ = unistd::close(fd);
        ProbeError::Unexpected(format!("{}", e))
    })?;
    if n == 0 {
        let _ = unistd::close(fd);
        return Err(ProbeError::Unexpected("connection closed".into()));
    }

    while let Some((tag, payload)) = sb.next() {
        if tag == Tag::Info && payload.len() == std::mem::size_of::<Info>() {
            let info: Info = unsafe { std::ptr::read(payload.as_ptr() as *const Info) };
            return Ok(ProbeResult { fd, info });
        }
    }

    let _ = unistd::close(fd);
    Err(ProbeError::Unexpected("no info response".into()))
}
