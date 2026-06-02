# ==============================================================================
# rift Terminal Session Daemon — Bash Aliases & Functions
# ==============================================================================

if command -v rift >/dev/null 2>&1; then

    # Project name = git repo basename, falling back to cwd basename. Used by
    # every `r*` shortcut so they all target the same session regardless of
    # which subdir of the repo you're in.
    __rift_project() {
        local root
        root=$(git rev-parse --show-toplevel 2>/dev/null)
        if [ -z "$root" ]; then
            root="$PWD"
        fi
        basename "$root"
    }

    # Internal helper to get session list
    __rift_sessions() {
        rift list -s
    }

    # Internal helper to pick a session with fzf
    __rift_pick() {
        local sessions
        sessions=$(__rift_sessions 2>/dev/null)
        if [ -z "$sessions" ]; then
            return 1
        fi
        echo "$sessions" | fzf --prompt="rift > " --height=40% --reverse
    }

    # ra: cd to a session's start directory and attach (defaults to project name)
    ra() {
        local session="${1:-$(__rift_project)}"
        local start_dir
        
        # Extract the start_dir of the session from rift list
        start_dir=$(rift list 2>/dev/null | awk -v name="$session" '
            $0 ~ "(^|[[:space:]])name="name"([[:space:]]|$)" {
                for (i=1; i<=NF; i++) {
                    if ($i ~ /^start_dir=/) {
                        sub(/^start_dir=/, "", $i);
                        print $i;
                        exit;
                    }
                }
            }
        ')
        
        # If the session exists and has a start directory, cd to it locally first
        if [ -n "$start_dir" ] && [ -d "$start_dir" ]; then
            cd "$start_dir" || return 1
        fi

        # Attach to the session
        rift attach "$session"
    }

    # rls: fzf-pick a rift session and attach in the current pane (with cd)
    rls() {
        local picked
        picked=$(__rift_pick)
        if [ -n "$picked" ]; then
            ra "$picked"
        fi
    }

    # rs: Attach to <project>.<role>: rs <role> (with cd)
    rs() {
        if [ $# -eq 0 ]; then
            echo "usage: rs <role>" >&2
            return 1
        fi
        ra "$(__rift_project).$1"
    }

    # rks: fzf-pick a rift session and kill it
    rks() {
        local picked
        picked=$(__rift_pick)
        if [ -n "$picked" ]; then
            rift kill "$picked"
        fi
    }

    # rssh: SSH into a host and attach to a rift session
    rssh() {
        if [ $# -lt 2 ]; then
            echo "usage: rssh <host> <session>" >&2
            return 1
        fi
        ssh -t "$1" rift attach "$2"
    }

    # Abbreviations/Aliases (single-quoted so __rift_project expands at execution time)
    alias rin='rift new "$(__rift_project)"'
    alias rir='rift run "$(__rift_project)"'
    alias ris='rift send "$(__rift_project)"'
    alias rd='rift detach'

fi
