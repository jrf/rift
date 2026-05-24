# rift

A terminal session daemon. Like tmux, screen, or abduco — but simpler.

rift keeps your shell alive when you disconnect. A background daemon owns a PTY and a shell; clients connect over Unix domain sockets. Detach with `Ctrl+\`, reattach later, and terminal state is restored.

Panes and tabs are left to your terminal emulator (WezTerm, Kitty, etc.). rift does one thing: persistent sessions.

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
rift <session>                Attach to (or create) a session
rift attach <session>         Same as above
rift attach -d <session>      Create session without attaching
rift new <session>            Same as attach -d
rift list [-s]                List sessions (-s for short/scriptable format)
rift run <session> <cmd...>   Run a command in a session (-d for detached, --fish)
rift send <session> <text>    Send keystrokes to a session
rift print <session> <text>   Inject text into session display
rift write <session> <path>   Write stdin to a file via the session
rift tail <name>...           Follow session output in real-time
rift history <session>        Print session output (--vt, --html)
rift detach [<session>]       Detach all clients (uses $RIFT_SESSION if no arg)
rift kill <name>... [-f]      Kill sessions (-f for SIGKILL)
rift wait <name>...           Wait for sessions to complete
rift completions <shell>      Print completions (bash, zsh, fish)
```

All subcommands have short aliases: `a`, `n`, `r`, `s`, `p`, `wr`, `t`, `hi`, `d`, `k`, `w`, `l`/`ls`, `c`, `v`, `h`.

**Detach key:** `Ctrl+\`

## Examples

```bash
# Start a session named "dev"
rift dev

# Detach with Ctrl+\, then reattach later
rift dev

# Run a command in the background, wait for it
rift run -d build make -j8
rift wait build

# Send keystrokes to a running session
rift send dev "ls -la" $'\n'

# Tail output from multiple sessions
rift tail 'dev*'

# List active sessions
rift list
```

## Environment Variables

| Variable | Description |
|---|---|
| `RIFT_SESSION` | Set inside sessions to the current session name |
| `RIFT_SESSION_PREFIX` | Prefix applied to session names (for grouping) |
| `RIFT_SHELL` | Override the shell to spawn (default: `$SHELL`, fallback: `/bin/sh`) |
| `RIFT_DIR` | Override the socket directory (default: `$XDG_RUNTIME_DIR/rift` or `$TMPDIR/rift-<uid>`) |
| `RIFT_DIR_MODE` | Permission mode for socket directory |
| `RIFT_LOG_MODE` | Permission mode for log files |

## Architecture

```
┌──────────┐     Unix socket     ┌──────────┐     PTY      ┌───────┐
│  Client   │◄──────────────────►│  Daemon   │◄────────────►│ Shell │
│ (rift)     │                    │ (forked)  │              │       │
└──────────┘                     └──────────┘              └───────┘
```

The daemon forks on first attach, creates a PTY, and spawns a shell. It runs a single `poll()` loop over: signal pipe, server socket, PTY master, and all client fds. Terminal state is tracked via a vt100 parser and replayed on reattach.

Sessions are identified by name and communicate over a binary protocol (5-byte header: 1 tag + 4 LE length + payload).

## Using with Terminal Emulators

rift intentionally has no built-in pane/tab system. Use your terminal emulator instead:

**WezTerm:**
```bash
wezterm cli split-pane -- rift attach dev.2
```

**Kitty:**
```bash
kitty @ launch --type=window --cwd=current rift attach dev.2
```

## License

MIT
