# rift

A terminal session daemon. Like tmux, screen, or abduco — but simpler.

rift keeps your shell alive when you disconnect. A background daemon owns a PTY and a shell; clients connect over Unix domain sockets. Detach with `Ctrl+\`, reattach later, and terminal state is restored.

Panes and tabs are left to your terminal emulator (WezTerm, Kitty, etc.). rift does one thing: persistent sessions.

## Install

### From a release (recommended)

Prebuilt binaries for macOS (arm64/x86_64) and Linux (arm64/x86_64) are attached
to every [tagged release](https://github.com/jrf/rift/releases).

```bash
# Homebrew (via tap or the bundled formula)
brew install --formula ./packaging/homebrew/rift.rb

# Mise (prebuilt binary via the ubi backend)
mise use ubi:jrf/rift        # or: mise use cargo:rift  (build from source)

# Cargo (build + install from crates/source)
cargo install --git https://github.com/jrf/rift
```

See [`packaging/`](packaging/) for the Homebrew formula, Mise metadata, and
the release process.

### From source

Requires Rust (edition 2024, ≥ 1.85) and [just](https://github.com/casey/just).

```bash
just              # release build + install to ~/.local/bin + codesign (macOS)
just build        # debug build
just release      # release build only
cargo build       # direct cargo
```

## Usage

```
rift                          Pick a session interactively ($RIFT_PICKER or builtin)
rift <name-or-command> [...]  Attach existing; otherwise run a PATH command or named shell
rift --new <command> [...]    Run command in next free basename session (name, name.1, ...)
rift attach <session>         Explicitly attach to (or create) a shell session
rift attach -d <session>      Create session without attaching
rift attach --labels "k=v ..." <session>  Attach/create with labels set atomically
rift new <session>            Same as attach -d
rift list [-s|-v] [--where k=v] List sessions, optionally filtered by label
rift get <session> [key]      Get all labels or one label value
rift set <session> k=v...     Set session labels (grouped "k=v k2=v2" also accepted)
rift unset <session> key...   Remove session labels
rift clear <session>          Clear all session labels
rift run <session> [<cmd...>] Run a command or piped script (-d for detached, --fish)
rift send <session> <text>    Send keystrokes to a session
rift print <session> <text>   Inject text into session display
rift write <session> <path>   Write stdin to a file via the session
rift tail <name>...           Follow session output in real-time
rift history <session>        Print session output (--vt, --html)
rift logs <session> [...]     Tail -f the session log file (extra args pass to tail)
rift last                     Attach to the most recently attached session
rift print-env [-s] <session> [key]  Print the leader client's tracked env vars (-s: export/unset)
rift detach [<session>]       Detach all clients (uses $RIFT_SESSION if no arg)
rift rename [<old_name>] <new_name> Rename a session (defaults to $RIFT_SESSION)
rift kill <name>... [-f]      Kill sessions (-f for SIGKILL)
rift wait <name>...           Wait for sessions to complete
rift completions <shell>      Print completions (bash, zsh, fish, nu)
```

All subcommands have short aliases: `a`, `n`, `r`, `s`, `p`, `wr`, `t`, `hi`, `lg`, `la`, `pe`, `d`, `rn`, `k`, `w`, `l`/`ls`, `c`, `v`, `h`. Commands that address the current session accept `.` as shorthand for `$RIFT_SESSION`.

**Detach key:** `Ctrl+\`

## Examples

```bash
# Start a session named "dev"
rift dev

# Start Codex in a session automatically named "codex"
rift codex

# Start another Codex session named "codex.1" (then codex.2, etc.)
rift --new codex

# Detach with Ctrl+\, then reattach later
rift dev

# Run a command in the background, wait for it
rift run -d build make -j8
rift wait build

# Run a multiline script from stdin
printf 'cargo fmt\ncargo test\n' | rift run checks

# Send keystrokes to a running session
rift send dev "ls -la" $'\n'

# Tail output from multiple sessions
rift tail 'dev*'

# List active sessions
rift list

# Label sessions and filter the list
rift set dev project=rift env=dev
rift list --where project=rift
```

For bare names, an existing session takes precedence over a command with the
same name. If no session exists, Rift runs an executable found in `PATH`; if no
executable is found and there are no arguments, it creates a shell session with
that name. Use `rift attach <name>` to always request shell/session behavior.
Rift subcommand names remain reserved; use `rift --new <command>` to run a
same-named executable.

## Environment Variables

| Variable | Description |
|---|---|
| `RIFT_SESSION` | Set inside sessions to the current session name |
| `RIFT_SESSION_PREFIX` | Prefix applied to session names (for grouping) |
| `RIFT_SHELL` | Override the shell to spawn (default: `$SHELL`, fallback: `/bin/sh`) |
| `RIFT_DIR` | Override the socket directory (default: `$XDG_RUNTIME_DIR/rift`, else `$HOME/.local/state/rift`) |
| `RIFT_DIR_MODE` | Permission mode for socket directory (default: `0700`) |
| `RIFT_LOG_MODE` | Permission mode for log files (default: `0600`) |
| `RIFT_EMPTY_TIMEOUT` | Idle duration (in seconds) after which a detached session with 0 clients will automatically terminate (e.g., `3600` for 1 hour) |
| `RIFT_NO_DETACH_KEY` | Disable the `Ctrl+\` detach shortcut when set; detach by closing the terminal or running `rift detach` |
| `RIFT_PICKER` | Shell command to use as session picker when `rift` is run with no args (e.g., `fzf`); receives session names on stdin, must print selection on stdout. Default: built-in numbered prompt. |
| `RIFT_TRACK_ENV` | Comma-separated list of environment variables an attaching client reports to the daemon for `rift print-env`. Default: `DISPLAY,SSH_AUTH_SOCK,SSH_AGENT_PID,SSH_CONNECTION,WINDOWID,XAUTHORITY,KITTY_LISTEN_ON,KITTY_PID,KITTY_WINDOW_ID` |
| `RIFT_ON_ATTACH` | Shell snippet run when a client attaches (fire-and-forget, stdio detached). `$RIFT_SESSION` is set and the session name is also passed as `$1`. |
| `RIFT_ON_DETACH` | Shell snippet run when a client detaches. Same context as `RIFT_ON_ATTACH`. |
| `RIFT_ON_EXIT` | Shell snippet run when the session's shell exits and the daemon tears down. Inherits the env present when the daemon was first spawned. |

## SSH Agent Forwarding

When attaching to a session from multiple SSH connections or after reconnecting, `rift` automatically and dynamically updates your `SSH_AUTH_SOCK` pointer. 

When the session is spawned, `rift` configures the shell's `SSH_AUTH_SOCK` to point to a stable symlink in your socket directory (`<socket_dir>/<session_name>.ssh-auth-sock`). Whenever a new `rift` client attaches, it sends its current SSH agent socket, and the daemon updates this symlink to point to the active agent. This allows commands (like `git push`) inside your persistent shell to seamlessly use your active SSH keys.

## Tracked Environment (`print-env`)

When a client attaches, it sends the daemon a snapshot of a small set of
environment variables — by default the display/SSH/terminal integration vars
`DISPLAY`, `SSH_AUTH_SOCK`, `SSH_AGENT_PID`, `SSH_CONNECTION`, `WINDOWID`,
`XAUTHORITY`, `KITTY_LISTEN_ON`, `KITTY_PID`, `KITTY_WINDOW_ID` (override the
list with `RIFT_TRACK_ENV`). The daemon keeps the interactive leader client's
snapshot so tools running *inside* the session can recover the current
terminal/GUI context after a reconnect:

```bash
rift print-env dev                 # KEY=VALUE lines (VALUE absent -> "-KEY")
rift print-env dev DISPLAY         # just one value (exit 1 if unset/absent)
eval "$(rift print-env -s dev)"    # re-export into the current shell
```

This mirrors reconnecting `SSH_AUTH_SOCK` above, but generalised: after moving
your session to a new terminal, `eval "$(rift print-env -s "$RIFT_SESSION")"`
refreshes `DISPLAY`/`KITTY_WINDOW_ID`/etc. for long-lived programs.

## Architecture

```
┌──────────┐     Unix socket     ┌──────────┐     PTY      ┌───────┐
│  Client   │◄──────────────────►│  Daemon   │◄────────────►│ Shell │
│ (rift)     │                    │ (forked)  │              │       │
└──────────┘                     └──────────┘              └───────┘
```

The daemon forks on first attach, creates a PTY, and spawns a shell. Both daemon and client run on a single-threaded tokio runtime (`current_thread` + `LocalSet`). The daemon's main task multiplexes the listening socket, PTY master (`AsyncFd<OwnedFd>`), and `SIGCHLD`/`SIGTERM` via `tokio::select!`; each accepted client is its own task that talks back through an mpsc channel. Terminal state is tracked with `alacritty_terminal` and replayed to reattaching clients.

When multiple interactive clients are attached, the client that most recently
sent keyboard input owns PTY resizing. One-shot `rift send` and terminal
responses do not take that ownership.

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

### Native Kitty integration

The no-UI kitten gives `cmd+shift+t` / `cmd+shift+enter` a scoped way to
spawn local or SSH-backed rift panes without enabling global Kitty remote
control. On Kitty 0.43 or newer, `cmd+shift+s` saves only rift-attached panes
in Kitty's native session format and `cmd+shift+r` restores them.

See [`scripts/README.md`](scripts/README.md) for the full layout, install
commands, and daily-use table.

### WezTerm layout save/restore

WezTerm has no native session format, so `scripts/wezterm/rift-wezterm`
reconstructs it: `save` records which rift session sits in each pane rectangle
(`wezterm cli list`), and `restore` rebuilds the splits with
`wezterm cli spawn`/`split-pane`, running `rift attach` in each pane so the
live daemons reconnect. A `scripts/wezterm/rift.lua` config module binds
`CMD+SHIFT+t/Enter` (new rift tab/split) and `CMD+SHIFT+s/r` (save/restore).

```bash
rift-wezterm save work        # snapshot current rift panes
rift-wezterm restore work     # rebuild them in a new window
```

See [`scripts/wezterm/README.md`](scripts/wezterm/README.md) for details.

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

MIT — see [LICENSE](LICENSE).
