# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Install

```bash
just              # default: release build + install to ~/.local/bin + codesign
just build        # debug build
just release      # release build
just install      # release build + cp to ~/.local/bin + codesign (macOS)
cargo build       # direct cargo
```

Zero warnings policy — all builds must be warning-free.

## Architecture

ryx is a terminal session daemon (like tmux/screen/abduco). A **daemon** process owns a PTY and a shell. **Clients** connect via Unix domain sockets to interact with the shell. Detaching leaves the daemon running; reattaching restores terminal state.

### Module Responsibilities

- **main.rs** — CLI parsing, daemon event loop, client event loop, PTY spawning, attach/detach/run flow, signal handling
- **ipc.rs** — Binary protocol: 5-byte header (1 tag + 4 LE length) + payload. `SocketBuffer` handles streaming/partial reads. `probe_session()` for health checks
- **socket.rs** — Unix domain socket lifecycle (create, connect, cleanup stale), session name validation, socket directory resolution (`RYX_DIR` > `XDG_RUNTIME_DIR/ryx` > `TMPDIR/ryx-{uid}`)
- **logger.rs** — File logger with 5MB rotation, implements `log::Log` trait. `Box::leak`'d for `&'static` lifetime
- **util.rs** — DA query/response handling, terminal state serialization (vt100), session listing, shell quoting, task exit marker detection, Kitty keyboard protocol

### Daemon Event Loop

Single `poll()` over: signal pipe, server socket, PTY master, all client fds. Handlers process in that order. The vt100::Parser tracks terminal state for reattach serialization.

### Key Design Decisions

- **Single fork daemonization** (not double-fork) — `setsid()` + redirect to `/dev/null`. Socket created before fork so child inherits it. Parent sleeps 10ms then connects (gives shell time for DA queries).
- **Self-pipe trick** — Signals (SIGCHLD, SIGTERM, SIGWINCH) write to a pipe fd that's included in poll(), avoiding async-signal-safety issues.
- **DA query drain** — Immediately after PTY spawn, daemon polls the master for up to 2s responding to DA1/DA2 queries. This prevents fish shell's 2-second timeout warning. Early output is preserved and fed to the parser.
- **First-attach skip** — `has_had_client` flag: Init (terminal state) is only sent on re-attach, not first attach, to avoid interfering with shell startup.
- **Detach key** — Ctrl+\ (0x1c). VQUIT is disabled in raw mode so the byte reaches the client instead of generating SIGQUIT.
- **Wire-compatible with zmx** — Same IPC protocol (tag enum values, header format) as the Zig-based zmx project.

### PTY Spawn Flow

`spawn_pty()` calls `libc::forkpty()`. Child sets `RYX_SESSION` env var, resets SIGPIPE, execs shell as login shell (`-fish`, `-zsh`, etc.). Shell is detected via `RYX_SHELL` > `SHELL` > `/bin/sh`.

### Gotchas

- nix 0.31 uses `AsFd` trait — raw fd access needs `BorrowedFd::borrow_raw()` wrappers
- Edition 2024 — `unsafe_op_in_unsafe_fn` is warn-by-default
- PTY master returns EIO (not EOF) when child exits
- macOS `sun_path` limit is 104 bytes (not 108 like Linux)
- poll() returns EINTR on signal delivery — always retry
- Client fds must be in the daemon's poll array or input will be delayed/lost
