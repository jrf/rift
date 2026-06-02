use std::io;
use std::os::unix::io::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
use nix::sys::socket::{
    self as nix_socket, bind, connect, listen, AddressFamily, Backlog, SockFlag, SockType, UnixAddr,
};
use nix::sys::stat::SFlag;

/// Maximum usable bytes in a Unix domain socket path (sun_path minus null terminator).
pub const MAX_SOCKET_PATH_LEN: usize = 104 - 1; // macOS sockaddr_un.sun_path is 104

pub fn session_prefix() -> String {
    std::env::var("RIFT_SESSION_PREFIX").unwrap_or_default()
}

pub fn session_name_from_env() -> String {
    std::env::var("RIFT_SESSION").unwrap_or_default()
}

#[derive(Debug)]
pub enum SessionNameError {
    Required,
    Invalid,
}

impl std::fmt::Display for SessionNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionNameError::Required => write!(f, "session name required"),
            SessionNameError::Invalid => write!(f, "invalid session name"),
        }
    }
}

/// Validate and combine prefix + session name.
/// Rejects `/`, null bytes, `.`, and `..`.
pub fn get_session_name(prefix: &str, name: &str) -> Result<String, SessionNameError> {
    if prefix.is_empty() && name.is_empty() {
        return Err(SessionNameError::Required);
    }
    let full = format!("{}{}", prefix, name);
    if full.contains('/') || full.contains('\0') || full == "." || full == ".." {
        return Err(SessionNameError::Invalid);
    }
    Ok(full)
}

/// Resolve the socket directory.
/// Priority: RIFT_DIR > XDG_RUNTIME_DIR/rift > $HOME/.local/state/rift > TMPDIR/rift-{uid}
///
/// $HOME is preferred over TMPDIR because macOS's TMPDIR varies between
/// GUI-launched processes (/var/folders/.../T/) and CLI/SSH ones (/tmp), so
/// using it splits sessions across two socket dirs that never see each other.
pub fn socket_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RIFT_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg).join("rift");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("state").join("rift");
    }
    let tmpdir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let tmpdir = tmpdir.trim_end_matches('/');
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("{}/rift-{}", tmpdir, uid))
}

#[derive(Debug)]
pub enum SocketPathError {
    NameTooLong,
}

impl std::fmt::Display for SocketPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "socket path too long")
    }
}

/// Build the full socket path and enforce sun_path length limit.
pub fn get_socket_path(socket_dir: &Path, session_name: &str) -> Result<PathBuf, SocketPathError> {
    let path = socket_dir.join(session_name);
    let path_bytes = path.as_os_str().as_encoded_bytes().len();
    if path_bytes > MAX_SOCKET_PATH_LEN {
        return Err(SocketPathError::NameTooLong);
    }
    Ok(path)
}

/// Returns the maximum session name length for a given socket directory,
/// or None if the socket directory path itself is already too long.
pub fn max_session_name_len(socket_dir: &Path) -> Option<usize> {
    let overhead = socket_dir.as_os_str().as_encoded_bytes().len() + 1;
    if overhead >= MAX_SOCKET_PATH_LEN {
        return None;
    }
    Some(MAX_SOCKET_PATH_LEN - overhead)
}

/// Print a descriptive error when a session name is too long.
pub fn print_session_name_too_long(session_name: &str, socket_dir: &Path) {
    if let Some(max_len) = max_session_name_len(socket_dir) {
        eprintln!(
            "error: session name is too long ({} bytes, max {} for socket directory \"{}\")",
            session_name.len(),
            max_len,
            socket_dir.display()
        );
    } else {
        eprintln!(
            "error: socket directory path is too long (\"{}\")",
            socket_dir.display()
        );
    }
}

/// Borrow a raw fd for use with nix APIs.
///
/// # Safety
/// The caller must ensure `fd` is a valid, open file descriptor for the
/// duration of the returned borrow.
unsafe fn borrow(fd: RawFd) -> BorrowedFd<'static> {
    unsafe { BorrowedFd::borrow_raw(fd) }
}

/// Set FD_CLOEXEC on a file descriptor.
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let b = unsafe { borrow(fd) };
    let flags = fcntl(&b, FcntlArg::F_GETFD).map_err(io_err)?;
    let flags = FdFlag::from_bits_truncate(flags);
    fcntl(&b, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC)).map_err(io_err)?;
    Ok(())
}

/// Set O_NONBLOCK on a file descriptor.
fn set_nonblock(fd: RawFd) -> io::Result<()> {
    let b = unsafe { borrow(fd) };
    let flags = fcntl(&b, FcntlArg::F_GETFL).map_err(io_err)?;
    let flags = OFlag::from_bits_truncate(flags);
    fcntl(&b, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io_err)?;
    Ok(())
}

/// Set both O_NONBLOCK and FD_CLOEXEC on a file descriptor.
pub fn set_nonblock_and_cloexec(fd: RawFd) -> io::Result<()> {
    set_nonblock(fd)?;
    set_cloexec(fd)?;
    Ok(())
}

/// Create a non-blocking Unix domain socket, bind, and listen.
pub fn create_socket(path: &Path) -> io::Result<OwnedFd> {
    let owned_fd = nix_socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(io_err)?;
    let fd = owned_fd.as_raw_fd();

    set_cloexec(fd)?;
    set_nonblock(fd)?;

    let addr = UnixAddr::new(path).map_err(io_err)?;
    bind(fd, &addr).map_err(io_err)?;
    listen(&owned_fd, Backlog::new(128).unwrap()).map_err(io_err)?;

    Ok(owned_fd)
}

/// Connect to an existing session's Unix socket (blocking, cloexec).
pub fn session_connect(socket_path: &str) -> io::Result<OwnedFd> {
    let owned_fd = nix_socket::socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(io_err)?;
    let fd = owned_fd.as_raw_fd();

    set_cloexec(fd)?;

    let addr = UnixAddr::new(socket_path).map_err(io_err)?;
    connect(fd, &addr).map_err(io_err)?;

    Ok(owned_fd)
}

/// Check if a session socket exists at the given path.
pub fn session_exists(dir: &Path, name: &str) -> io::Result<bool> {
    let path = dir.join(name);
    match nix::sys::stat::stat(&path) {
        Ok(stat) => {
            if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFSOCK {
                Ok(true)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file is not a unix socket",
                ))
            }
        }
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(e) => Err(io_err(e)),
    }
}

/// Remove a stale socket file.
pub fn cleanup_stale_socket(dir: &Path, session_name: &str) {
    log::warn!("stale socket found, cleaning up session={}", session_name);
    let path = dir.join(session_name);
    if let Err(e) = std::fs::remove_file(&path) {
        log::warn!("failed to delete stale socket: {}", e);
    }
}

/// Create the socket directory and logs subdirectory if they don't exist.
pub fn ensure_dirs(socket_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(socket_dir)?;
    std::fs::create_dir_all(socket_dir.join("logs"))?;
    use std::os::unix::fs::PermissionsExt;
    let dir_mode = parse_mode_env("RIFT_DIR_MODE", 0o750);
    let log_mode = parse_mode_env("RIFT_LOG_MODE", 0o640);
    let _ = std::fs::set_permissions(socket_dir, std::fs::Permissions::from_mode(dir_mode));
    let _ = std::fs::set_permissions(
        socket_dir.join("logs"),
        std::fs::Permissions::from_mode(dir_mode),
    );
    // Apply log mode to existing log files
    if let Ok(entries) = std::fs::read_dir(socket_dir.join("logs")) {
        for entry in entries.flatten() {
            let _ =
                std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(log_mode));
        }
    }
    Ok(())
}

fn parse_mode_env(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|s| {
            u32::from_str_radix(s.trim_start_matches("0o").trim_start_matches("0"), 8).ok()
        })
        .unwrap_or(default)
}

fn io_err(e: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

/// Update the SSH_AUTH_SOCK symlink to point to the client's active agent socket.
pub fn update_ssh_auth_sock_symlink(socket_dir: &Path, session_name: &str, target_path: &str) {
    let symlink_path = socket_dir.join(format!("{}.ssh-auth-sock", session_name));
    if symlink_path.exists() || symlink_path.is_symlink() {
        let _ = std::fs::remove_file(&symlink_path);
    }
    if !target_path.is_empty() {
        let _ = std::os::unix::fs::symlink(target_path, &symlink_path);
    }
}
