# rif

A terminal session daemon. Like tmux, screen, or abduco — but simpler.

rif keeps your shell alive when you disconnect. A background daemon owns a PTY and a shell; clients connect over Unix domain sockets. Detach with `Ctrl+\`, reattach later, and terminal state is restored.

Panes and tabs are left to your terminal emulator (WezTerm, Kitty, etc.). rif does one thing: persistent sessions.

## Install

Requires Rust (edition 2024) and [just](https://github.com/casey/just).

```bash
just              # release build + install to ~/.local/bin + codesign (macOS)
just build        # debug build
just release      # release build only
cargo build       # direct cargo
```

## Usage

```
rif <session>                Attach to (or create) a session
rif attach <session>         Same as above
rif attach -d <session>      Create session without attaching
rif new <session>            Same as attach -d
rif list [-s]                List sessions (-s for short/scriptable format)
rif run <session> <cmd...>   Run a command in a session (-d for detached, --fish)
rif send <session> <text>    Send keystrokes to a session
rif print <session> <text>   Inject text into session display
rif write <session> <path>   Write stdin to a file via the session
rif tail <name>...           Follow session output in real-time
rif history <session>        Print session output (--vt, --html)
rif detach [<session>]       Detach all clients (uses $RIF_SESSION if no arg)
rif kill <name>... [-f]      Kill sessions (-f for SIGKILL)
rif wait <name>...           Wait for sessions to complete
rif completions <shell>      Print completions (bash, zsh, fish)
```

All subcommands have short aliases: `a`, `n`, `r`, `s`, `p`, `wr`, `t`, `hi`, `d`, `k`, `w`, `l`/`ls`, `c`, `v`, `h`.

**Detach key:** `Ctrl+\`

## Examples

```bash
# Start a session named "dev"
rif dev

# Detach with Ctrl+\, then reattach later
rif dev

# Run a command in the background, wait for it
rif run -d build make -j8
rif wait build

# Send keystrokes to a running session
rif send dev "ls -la" $'\n'

# Tail output from multiple sessions
rif tail 'dev*'

# List active sessions
rif list
```

## Environment Variables

| Variable | Description |
|---|---|
| `RIF_SESSION` | Set inside sessions to the current session name |
| `RIF_SESSION_PREFIX` | Prefix applied to session names (for grouping) |
| `RIF_SHELL` | Override the shell to spawn (default: `$SHELL`, fallback: `/bin/sh`) |
| `RIF_DIR` | Override the socket directory (default: `$XDG_RUNTIME_DIR/rif` or `$TMPDIR/rif-<uid>`) |
| `RIF_DIR_MODE` | Permission mode for socket directory |
| `RIF_LOG_MODE` | Permission mode for log files |

## Architecture

```
┌──────────┐     Unix socket     ┌──────────┐     PTY      ┌───────┐
│  Client   │◄──────────────────►│  Daemon   │◄────────────►│ Shell │
│ (rif)     │                    │ (forked)  │              │       │
└──────────┘                     └──────────┘              └───────┘
```

The daemon forks on first attach, creates a PTY, and spawns a shell. It runs a single `poll()` loop over: signal pipe, server socket, PTY master, and all client fds. Terminal state is tracked via a vt100 parser and replayed on reattach.

Sessions are identified by name and communicate over a binary protocol (5-byte header: 1 tag + 4 LE length + payload).

## Using with Terminal Emulators

rif intentionally has no built-in pane/tab system. Use your terminal emulator instead:

**WezTerm:**
```bash
wezterm cli split-pane -- rif attach dev.2
```

**Kitty:**
```bash
kitty @ launch --type=window --cwd=current rif attach dev.2
```

## License

MIT
