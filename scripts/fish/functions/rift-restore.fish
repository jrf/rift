# Symlink into ~/.config/fish/functions/rift-restore.fish

function rift-restore --description 'Reattach to all rift sessions, local or remote. Usage: rift-restore [--split] [host]'
    argparse --name=rift-restore 's/split' -- $argv
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

    if test -z "$sessions"
        if test -n "$host"
            echo "no rift sessions on $host" >&2
        else
            echo "no local rift sessions" >&2
        end
        return 1
    end

    for session in $sessions
        if test -n "$host"
            kitten @ launch $placement ssh -t $host rift attach $session
        else
            kitten @ launch $placement rift attach $session
        end
    end
    echo "reattached to "(count $sessions)" session(s)"
end
