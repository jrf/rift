function rift-pick --description 'fzf-pick a rift session or saved layout. Usage: rift-pick [--split] [--single] [host]'
    argparse --name=rift-pick 's/split' '1/single' 'L/no-layout' -- $argv
    or return 1
    set -l placement --type=tab
    if set -q _flag_split
        set placement --location=split
    end

    set -l host
    set -l sessions
    if test (count $argv) -eq 0
        set sessions (rift list -s 2>/dev/null)
    else
        set host $argv[1]
        set sessions (ssh $host 'bash -lc "rift list -s"' 2>/dev/null)
    end

    set -l snapshots_dir ~/.local/state/rift
    set -l layouts
    if test -z "$host"; and test -d $snapshots_dir
        for f in $snapshots_dir/*.kitty
            test -f $f; and set -a layouts (string replace -r '\.kitty$' '' (basename $f))
        end
    end

    set -l items
    for s in $sessions
        set -a items "session  $s"
    end
    for l in $layouts
        set -a items "layout   $l"
    end

    if test (count $items) -eq 0
        echo "nothing to pick" >&2
        return 1
    end

    set -l prompt "rift"
    test -n "$host"; and set prompt "rift @ $host"
    set -l picked (printf "%s\n" $items | fzf --prompt="$prompt > " --height=40% --reverse)
    test -z "$picked"; and return 1

    set -l kind (string split -m 1 ' ' -- $picked)[1]
    set -l name (string trim -- (string sub -s (math (string length $kind) + 2) -- $picked))

    switch $kind
        case session
            # If a saved snapshot contains this session, prefer it — the
            # snapshot has the original tab/split layout, sibling list, etc.
            if not set -q _flag_no_layout; and test -z "$host"; and test -d $snapshots_dir
                set -l escaped (string escape --style=regex -- $name)
                for f in $snapshots_dir/*.kitty
                    test -f $f; or continue
                    if grep -Eq " rift attach $escaped\$" $f
                        rift-load-snapshot $f
                        return
                    end
                end
            end

            # Otherwise restore the sibling group as tabs/splits.
            set -l to_attach $name
            if not set -q _flag_single
                if string match -rq '\.[0-9]+$' -- $name
                    set -l project (string replace -r '\.[0-9]+$' '' -- $name)
                    set to_attach
                    for s in $sessions
                        if string match -rq "^"(string escape --style=regex -- $project)"\.[0-9]+\$" -- $s
                            set -a to_attach $s
                        end
                    end
                end
            end
            for session in $to_attach
                if test -n "$host"
                    kitten @ launch $placement ssh -t $host rift attach $session
                else
                    kitten @ launch $placement rift attach $session
                end
            end
        case layout
            rift-load-snapshot $name
    end
end
