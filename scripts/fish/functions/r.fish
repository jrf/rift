# Symlink into ~/.config/fish/functions/r.fish

function r --description 'Attach to a rift pane on a remote host: r <host> [project]'
    if test (count $argv) -eq 0
        echo "usage: r <host> [project]" >&2
        return 1
    end
    set -l host $argv[1]
    set -e argv[1]
    ssh -t $host bash -lc "rift-pane $argv"
end
