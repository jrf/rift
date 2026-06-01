#!/usr/bin/env python3
"""Read `kitten @ ls` JSON on stdin and emit a kitty session file that
recreates the layout of every window that's currently running a rift
attach (or ssh ... rift attach).

Non-rift panes are skipped. Tab/window structure is preserved; pane sizing
is not (kitty's session format doesn't carry exact dimensions, only the
splits layout's stack order).
"""

import json
import shlex
import sys


def is_rift_window(window):
    for proc in window.get("foreground_processes", []) or []:
        cmdline = proc.get("cmdline") or []
        if not cmdline:
            continue
        if "rift" in cmdline:
            return True
        # Detect `ssh ... rift attach ...` chains
        if "rift" in " ".join(cmdline) and (
            cmdline[0].endswith("ssh") or cmdline[0].endswith("kitten")
        ):
            return True
    return False


def window_cmdline(window):
    """Pick the cmdline of the deepest rift-related process (so we capture
    `rift attach foo.0` rather than the outer login shell)."""
    best = None
    for proc in window.get("foreground_processes", []) or []:
        cmdline = proc.get("cmdline") or []
        if cmdline and ("rift" in cmdline or "rift" in " ".join(cmdline)):
            best = cmdline
    return best


def main():
    data = json.load(sys.stdin)
    out = []
    first_tab = True

    for os_win in data:
        for tab in os_win.get("tabs", []):
            rift_windows = [w for w in tab.get("windows", []) if is_rift_window(w)]
            if not rift_windows:
                continue

            if not first_tab:
                out.append("")
            first_tab = False

            title = tab.get("title", "").strip() or "rift"
            out.append(f"new_tab {title}")
            out.append(f"layout {tab.get('layout', 'splits')}")

            # Alternate vsplit/hsplit to approximate btree placement.
            # (`--location=split` in session files doesn't dynamically pick
            # the focused window's longer axis the way runtime does.)
            for i, w in enumerate(rift_windows):
                cmdline = window_cmdline(w)
                if cmdline is None:
                    continue
                cmd = " ".join(shlex.quote(c) for c in cmdline)
                if i == 0:
                    location = ""
                elif i % 2 == 1:
                    location = " --location=vsplit"
                else:
                    location = " --location=hsplit"
                out.append(f"launch{location} {cmd}")

    if not out:
        sys.stderr.write("no rift sessions found in current kitty layout\n")
        sys.exit(1)
    print("\n".join(out))


if __name__ == "__main__":
    main()
