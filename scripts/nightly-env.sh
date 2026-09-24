# Shared by nightly.sh and nightly-mecha.sh — sourced, never run.
#
# nightly_env_status FILE prints one of:
#   absent       no file: defaults apply
#   ok           sources cleanly
#   unreadable   exists but cannot be read (or is not a regular file)
#   unparseable  a syntax error
#   aborts       sourcing kills the shell (e.g. `set -u` on an unset variable)
#
# The two nightlies must reach the SAME answer about one file, or an
# off-switch written there works in one half only — and the lane it switches
# off makes permanent rejects. A last line that merely returns false (a
# `[ -f x ] && FOO=…` whose test fails) is a fine file: `.` reports that
# status, so the source's exit code cannot be the test. A sentinel printed
# after the source in the same shell can — only a real abort skips it.
#
# The probe runs in a CLEAN environment (HOME, PATH, and MECHA_GRAPH_DIR as
# the file's own directory, under `set -u`), never the caller's: the two
# nightlies define different variables before they ask, so probing in their
# live state would let one file that names a variable only one of them sets
# get two answers. A shared rule must also be a shared answer.
nightly_env_clean() {
    env -i HOME="$HOME" PATH="$PATH" MECHA_GRAPH_DIR="$(dirname "$1")" bash -u -c "$2" _ "$1" "${3:-}"
}
nightly_env_status() {
    local f=$1
    # A dangling symlink fails `-e`, but it is a file the operator believes
    # is there — `absent` would take the defaults silently, and the default
    # runs the lane that makes permanent rejects.
    [ -e "$f" ] || [ -L "$f" ] || { echo absent; return; }
    { [ -f "$f" ] && [ -r "$f" ]; } || { echo unreadable; return; }
    bash -n "$f" 2>/dev/null || { echo unparseable; return; }
    [ "$(nightly_env_clean "$f" '. "$1" >/dev/null 2>&1; echo __sourced__')" = "__sourced__" ] \
        || { echo aborts; return; }
    echo ok
}

# The value NAME takes in FILE, read in the same clean environment the
# status probe used; empty when the file leaves it unset.
nightly_env_value() {
    nightly_env_clean "$1" '. "$1" >/dev/null 2>&1; printf "%s" "${!2:-}"' "$2"
}
