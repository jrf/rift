function rift-snapshot --description 'Save current kitty rift layout. Usage: rift-snapshot [name|path]'
    set -l target $argv[1]
    set -l snapshots_dir ~/.local/state/rift

    if test -z "$target"
        set target snapshot
    end

    set -l out
    if string match -q '*/*' -- $target
        set out $target
    else
        set out $snapshots_dir/$target.kitty
    end

    mkdir -p (dirname $out)
    if not kitten @ ls | python3 ~/.dotfiles/config/kitty/rift-snapshot.py >$out
        rm -f $out
        return 1
    end
    set -l panes (grep -c '^launch ' $out)
    echo "saved → $out ($panes panes)"
end
