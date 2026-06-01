# Symlink into ~/.config/fish/conf.d/rift-autostart.fish

if set -q RIFT_AUTOSTART
    set -e RIFT_AUTOSTART
    # In case the cloned shell inherited a stale RIFT_SESSION (from the
    # source pane), drop it so `rift new` inside rift-pane doesn't bail
    # with "already inside session".
    set -e RIFT_SESSION
    exec rift-pane
end
