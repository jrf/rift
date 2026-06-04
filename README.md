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
rift                          Pick a session interactively ($RIFT_PICKER or builtin)
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
rift logs <session> [...]     Tail -f the session log file (extra args pass to tail)
rift last                     Attach to the most recently attached session
rift detach [<session>]       Detach all clients (uses $RIFT_SESSION if no arg)
rift rename [<old_name>] <new_name> Rename a session (defaults to $RIFT_SESSION)
rift kill <name>... [-f]      Kill sessions (-f for SIGKILL)
rift wait <name>...           Wait for sessions to complete
rift completions <shell>      Print completions (bash, zsh, fish)
```

All subcommands have short aliases: `a`, `n`, `r`, `s`, `p`, `wr`, `t`, `hi`, `lg`, `la`, `d`, `rn`, `k`, `w`, `l`/`ls`, `c`, `v`, `h`.

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
| `RIFT_DIR` | Override the socket directory (default: `$XDG_RUNTIME_DIR/rift`, else `$HOME/.local/state/rift`) |
| `RIFT_DIR_MODE` | Permission mode for socket directory |
| `RIFT_LOG_MODE` | Permission mode for log files |
| `RIFT_EMPTY_TIMEOUT` | Idle duration (in seconds) after which a detached session with 0 clients will automatically terminate (e.g., `3600` for 1 hour) |
| `RIFT_PICKER` | Shell command to use as session picker when `rift` is run with no args (e.g., `fzf`); receives session names on stdin, must print selection on stdout. Default: built-in numbered prompt. |
| `RIFT_ON_ATTACH` | Shell snippet run when a client attaches (fire-and-forget, stdio detached). `$RIFT_SESSION` is set and the session name is also passed as `$1`. |
| `RIFT_ON_DETACH` | Shell snippet run when a client detaches. Same context as `RIFT_ON_ATTACH`. |
| `RIFT_ON_EXIT` | Shell snippet run when the session's shell exits and the daemon tears down. Inherits the env present when the daemon was first spawned. |

## SSH Agent Forwarding

When attaching to a session from multiple SSH connections or after reconnecting, `rift` automatically and dynamically updates your `SSH_AUTH_SOCK` pointer. 

When the session is spawned, `rift` configures the shell's `SSH_AUTH_SOCK` to point to a stable symlink in your socket directory (`<socket_dir>/<session_name>.ssh-auth-sock`). Whenever a new `rift` client attaches, it sends its current SSH agent socket, and the daemon updates this symlink to point to the active agent. This allows commands (like `git push`) inside your persistent shell to seamlessly use your active SSH keys.

## Architecture

```
┌──────────┐     Unix socket     ┌──────────┐     PTY      ┌───────┐
│  Client   │◄──────────────────►│  Daemon   │◄────────────►│ Shell │
│ (rift)     │                    │ (forked)  │              │       │
└──────────┘                     └──────────┘              └───────┘
```

The daemon forks on first attach, creates a PTY, and spawns a shell. Both daemon and client run on a single-threaded tokio runtime (`current_thread` + `LocalSet`). The daemon's main task multiplexes the listening socket, PTY master (`AsyncFd<OwnedFd>`), and `SIGCHLD`/`SIGTERM` via `tokio::select!`; each accepted client is its own task that talks back through an mpsc channel. Terminal state is tracked via a vt100 parser and replayed to reattaching clients.

Sessions are identified by name and communicate over a binary protocol (5-byte header: 1 tag + 4 LE length + payload). Framing is handled by `tokio-util::codec` (`ipc::RiftCodec`).

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

### Recipe: keybindings that spawn fresh rift panes (local & SSH)

A drop-in kitty + fish + bash setup that gives you `cmd+shift+t` /
`cmd+shift+enter` to spawn a new rift-attached pane — locally *or* SSH'd back
to the host you're already on — plus `rift-restore <host>` for re-establishing
every remote session after a local reboot.

See [`scripts/README.md`](scripts/README.md) for the full layout, install
commands, and daily-use table.

## Integrating with SSH Login

To automatically start or connect to a default `rift` session (e.g., named "main") every time you connect to a server over SSH, you can add the following snippet to your shell configuration (`~/.bashrc`, `~/.zshrc`, or `~/.profile`):

```bash
# Automatically launch/attach to a default 'rift' session on SSH login
if [ -n "$SSH_CONNECTION" ] && [ -z "$RIFT_SESSION" ] && command -v rift >/dev/null 2>&1; then
    exec rift main
fi
```

This ensures that when you disconnect or lose your SSH connection, your processes remain running in the background, and the next time you SSH in, you will be immediately reattached to your persistent "main" session.

## License

MIT
