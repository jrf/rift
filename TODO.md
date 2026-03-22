# ryx — Terminal session daemon

## Now
- [ ] `src/completions.rs` — Shell completion scripts (bash, zsh, fish) #feature

## Next

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
- [x] Detached spawn — `ryx new <session>` or `ryx attach -d <session>` #feature
- [x] wait — poll sessions for task completion, prefix matching, aggregate exit codes #feature
