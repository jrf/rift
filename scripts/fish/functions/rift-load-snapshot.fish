function rift-load-snapshot --description 'Open a saved kitty rift layout in a new OS window. Usage: rift-load-snapshot [name|path]'
    set -l target $argv[1]
    set -l snapshots_dir ~/.local/state/rift

    if test -z "$target"
        set target snapshot
    end

    set -l file
    if string match -q '*/*' -- $target
        set file $target
    else
        set file $snapshots_dir/$target.kitty
    end

    if not test -f $file
        echo "no snapshot at $file" >&2
        return 1
    end
    open -na kitty --args --session $file
end
