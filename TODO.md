# rift — Terminal session daemon

## Now

## Next

## Later

## Scrapped

## Done
- [x] Rename sessions — `rift rename [<old_name>] <new_name>` (alias `rn`) dynamically renames active sessions, including Unix sockets, logs, and symlinks #feature
- [x] Migrate daemon and client to async tokio runtime (current_thread + LocalSet, per-client tasks via mpsc, `tokio-util` codec for the wire protocol) #refactor
- [x] Use deterministic socket dir (`$HOME/.local/state/rift`) so macOS GUI- vs CLI-launched shells share the same sessions #bug
- [x] Fix rift tail escape sequence rendering and Ctrl+C interrupt bug #bug
- [x] Fix session listing bug where .ssh-auth-sock symlinks were erroneously listed as unreachable sessions #bug
- [x] Optimize session startup performance (reduced from 2s to ~15ms) by removing busy-waiting in PTY DA query draining #improvement
- [x] SSH agent forwarding socket symlink propagation for multiple SSH sessions #feature
- [x] Cargo project initialized with deps: nix, vt100, log, env_logger, libc #chore
- [x] `src/ipc.rs` — IPC protocol (Tag enum, Header, send/recv, SocketBuffer, probe_session) #feature
- [x] `src/socket.rs` — Unix socket creation/connection, session name validation, path management #feature
- [x] `src/logger.rs` — File-based logging with 5MB rotation #feature
- [x] `src/util.rs` — Shell quoting, DA responses, task exit markers, terminal serialization (vt100), session listing #feature
- [x] `src/main.rs` — Core runtime: CLI, PTY spawning, daemon/client event loops, attach flow #feature
- [x] list — enumerate active sessions with status info #feature
- [x] kill — terminate a session by name #feature
- [x] detach — disconnect all clients from a session #feature
- [x] history — retrieve session output (plain, --vt, --html) #feature
- [x] Detached spawn — `rift new <session>` or `rift attach -d <session>` #feature
- [x] wait — poll sessions for task completion, prefix matching, aggregate exit codes #feature
- [x] completions — shell completion scripts for bash, zsh, fish via `rift completions <shell>` #feature
- [x] `send` — inject keystrokes into a session's PTY input (fire-and-forget, stdin support) #feature
- [x] `tail` — follow session output in real-time #feature
- [x] `run -d` — detached/background run, track with `wait` #feature
- [x] `kill --force` / `-f` — SIGKILL instead of SIGTERM #feature
- [x] Short aliases for all subcommands (`a`, `r`, `s`, `d`, `l`/`ls`, `k`, `hi`, `w`, `t`, `c`, `v`, `h`) #improvement
- [x] `detach` without args uses `RIFT_SESSION` from env #improvement
- [x] Fix: client now exits on daemon-initiated detach (DetachAll) #bug
- [x] Fix: daemon reads pending data before treating POLLHUP as disconnect #bug
- [x] `kill` with multiple names and prefix matching (`rift kill dev*`) #improvement
- [x] `tail` with multiple names and prefix matching (`rift tail dev*`) #improvement
- [x] `RIFT_SESSION_PREFIX` applied to `kill` and `tail` #improvement
- [x] `print` — inject text into session display without PTY input #feature
- [x] `write` — pipe stdin to a file via session (base64 chunked) #feature
- [x] `attach <session> <command>` — spawn specific command instead of login shell #feature
- [x] `run --fish` — fish-specific command completion detection #improvement
- [x] `RIFT_DIR_MODE` / `RIFT_LOG_MODE` — permission modes for socket dirs and log files #feature
