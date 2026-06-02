//! Daemon module: the long-lived background process that owns a PTY and
//! brokers multiple clients, plus the per-client process that proxies a
//! local terminal to the daemon over a Unix socket.
//!
//! - [`server`] — the daemon-process side (forking, event loop, vt100 state).
//! - [`client`] — the client-process side (raw mode, signal/codec plumbing).
//!
//! Both run on a single-threaded tokio runtime (`current_thread` + `LocalSet`);
//! see the module docs for details.

use std::path::PathBuf;

use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

use crate::socket;

mod client;
mod server;

pub use client::run_client;
pub use server::{spawn_daemon, spawn_daemon_detached};

// ---------------------------------------------------------------------------
// Cfg — session configuration
// ---------------------------------------------------------------------------

pub struct Cfg {
    pub session_name: String,
    pub socket_dir: PathBuf,
    pub socket_path: PathBuf,
}

impl Cfg {
    pub fn resolve(name: &str) -> Result<Self, String> {
        let prefix = socket::session_prefix();
        let session_name = socket::get_session_name(&prefix, name).map_err(|e| format!("{}", e))?;
        let socket_dir = socket::socket_dir();
        let socket_path = socket::get_socket_path(&socket_dir, &session_name).map_err(|_| {
            socket::print_session_name_too_long(&session_name, &socket_dir);
            "socket path too long".to_string()
        })?;
        Ok(Cfg {
            session_name,
            socket_dir,
            socket_path,
        })
    }
}

// ---------------------------------------------------------------------------
// Shared signal helper (used by both daemon and client processes and from
// commands.rs for one-shot SIGPIPE ignoring).
// ---------------------------------------------------------------------------

pub fn ignore_signal(sig: Signal) {
    let sa = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    unsafe {
        let _ = sigaction(sig, &sa);
    }
}
