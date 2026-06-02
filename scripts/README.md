# scripts/

End-to-end integration recipe for using rift with kitty and ssh: one
keybinding (`cmd+shift+t` for tab, `cmd+shift+enter` for split) that spawns
a new rift-attached pane *either* locally *or* SSH'd back to the host you're
already on, plus a `rift-restore <host>` helper for re-establishing every
remote session after a local reboot.

## Layout

| Path | Purpose |
|---|---|
| `rift-pane` | Allocates the next free `<basename($PWD)>.<N>` session and attaches. Lives in `~/.local/bin/` or `~/.bin/`. |
| `kitty/rift_spawn.py` | Kitty kitten: detects whether the source pane is ssh'd and dispatches to either a local `launch` or an ssh-back-and-`rift-pane`. Lives in `~/.config/kitty/`. |
| `kitty/bindings.conf` | The lines to add to `~/.config/kitty/kitty.conf`. |
| `fish/conf.d/rift-autostart.fish` | When a cloned shell starts with `RIFT_AUTOSTART=1`, exec rift-pane. Symlink into `~/.config/fish/conf.d/`. |
| `fish/functions/r.fish` | `r <host> [project]` — attach to a rift pane on a remote host from a cold local shell. |
| `fish/functions/rift-restore.fish` | `rift-restore [host]` — open a local tab (or split with `--split`) for each rift session, local or on the remote. |
| `fish/functions/rift-pick.fish` | `rift-pick [host]` — fzf-pick a session and attach in a new tab/split. Picking `foo.<N>` also restores siblings (`--single` to disable). |
| `fish/functions/rift-snapshot.fish` | `rift-snapshot [file]` — write a kitty session file capturing the current tab/split layout of every rift-attached pane. |
| `fish/functions/rift-load-snapshot.fish` | `rift-load-snapshot [file]` — open a new OS window from a saved snapshot. |
| `kitty/rift-snapshot.py` | Helper invoked by `rift-snapshot` to turn `kitten @ ls` JSON into a kitty session file. |
| `bash/rift-autostart.bash` | The equivalent autostart hook for remote bash. Append to the *top* of `~/.bashrc` on every host you ssh into. |
| `bash/rift-aliases.bash` | The complete set of bash aliases and functions. |

## Install (one-time, per machine)

```bash
# rift binary (built locally)
just install                                  # → ~/.local/bin/rift

# rift-pane script
install -m 0755 scripts/rift-pane ~/.local/bin/

# kitty kitten + bindings
cp scripts/kitty/rift_spawn.py ~/.config/kitty/
cat scripts/kitty/bindings.conf >> ~/.config/kitty/kitty.conf

# fish (adjust if you use a different shell)
ln -s "$PWD/scripts/fish/conf.d/rift-autostart.fish"     ~/.config/fish/conf.d/
ln -s "$PWD/scripts/fish/functions/r.fish"                ~/.config/fish/functions/
ln -s "$PWD/scripts/fish/functions/rift-restore.fish"     ~/.config/fish/functions/
ln -s "$PWD/scripts/fish/functions/rift-pick.fish"        ~/.config/fish/functions/

# bash (if you use bash instead of fish)
# ln -s "$PWD/scripts/bash/rift-aliases.bash"            ~/.bash_aliases

# on each remote you ssh into:
scp scripts/rift-pane <host>:~/.local/bin/
ssh <host> chmod +x ~/.local/bin/rift-pane
cat scripts/bash/rift-autostart.bash | ssh <host> 'cat >> ~/.bashrc'
# (also ensure `rift` itself is built/installed on the remote and on the
#  login PATH; check with `ssh <host> 'bash -lc "which rift rift-pane"'`)
```

## Reload

```bash
kitty @ load-config-file        # pick up new kitty bindings
exec fish                       # pick up new fish functions/conf.d
```

## Daily use

| Where you are | Press | What you get |
|---|---|---|
| Local kitty pane (any cwd) | `cmd+shift+t` / `cmd+shift+enter` | New local tab/split, fresh rift session named `<cwd>.<N>` |
| Local kitty pane, ssh'd into a host | same | New local tab/split, SSH'd back, fresh rift session on the remote |
| Cold local shell (no kitty pane needed) | `r <host>` | SSH + new rift session |
| After a local reboot | `rift-restore <host>` | One local tab per existing remote session |
| Want to grab one specific session | `rift-pick [host]` | fzf prompt; picking `foo.N` brings back all `foo.*` siblings |
| Save the current layout before quitting kitty | `rift-snapshot` | writes `~/.local/state/rift/snapshot.kitty` |
| Restore a saved layout in a new OS window | `rift-load-snapshot` | exact tab/split structure, each pane re-attached |

## Notes

- The kitten extracts the remote host from the running ssh process's argv.
  It recognises plain `ssh` and `kitten ssh` (kitty's ssh kitten).
- Remote cwd is *not* preserved when spawning a sibling SSH'd pane —
  rift-pane on the remote uses `$HOME`'s basename for the session name.
- `RIFT_DIR` defaults to `$HOME/.local/state/rift`, so sessions are visible
  the same way from any shell on the host (no macOS `$TMPDIR` confusion).
