# rif — Terminal session daemon

## Now

## Next
- [ ] `print` — inject text into session display without PTY input #feature
- [ ] `write` — pipe stdin to a file via session (base64 chunked) #feature
- [ ] `attach <session> <command>` — spawn specific command instead of login shell #feature
- [ ] `run --fish` — fish-specific command completion detection #improvement
- [ ] `RIF_DIR_MODE` / `RIF_LOG_MODE` — permission modes for socket dirs and log files #feature

## Later

## Scrapped

## Done
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
- [x] Detached spawn — `rif new <session>` or `rif attach -d <session>` #feature
- [x] wait — poll sessions for task completion, prefix matching, aggregate exit codes #feature
- [x] completions — shell completion scripts for bash, zsh, fish via `rif completions <shell>` #feature
- [x] `send` — inject keystrokes into a session's PTY input (fire-and-forget, stdin support) #feature
- [x] `tail` — follow session output in real-time #feature
- [x] `run -d` — detached/background run, track with `wait` #feature
- [x] `kill --force` / `-f` — SIGKILL instead of SIGTERM #feature
- [x] Short aliases for all subcommands (`a`, `r`, `s`, `d`, `l`/`ls`, `k`, `hi`, `w`, `t`, `c`, `v`, `h`) #improvement
- [x] `detach` without args uses `RIF_SESSION` from env #improvement
- [x] Fix: client now exits on daemon-initiated detach (DetachAll) #bug
- [x] Fix: daemon reads pending data before treating POLLHUP as disconnect #bug
