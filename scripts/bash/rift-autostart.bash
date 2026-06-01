# Append to the TOP of ~/.bashrc on every host you want rift-pane autostart
# to fire on (typically every remote you ssh into).
#
# When a new bash starts with RIFT_AUTOSTART=1 in its env, this block exec's
# rift-pane (which attaches to a fresh rift session). Without that env var
# the block is a no-op, so it has zero impact on normal interactive shells.

if [ "$RIFT_AUTOSTART" = "1" ]; then
    unset RIFT_AUTOSTART RIFT_SESSION
    PATH="$HOME/.local/bin:$HOME/.bin:$PATH"
    exec rift-pane
fi
