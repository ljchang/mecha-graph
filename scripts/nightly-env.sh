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
# after the source in the same subshell can — only a real abort skips it.
nightly_env_status() {
    local f=$1
    [ -e "$f" ] || { echo absent; return; }
    { [ -f "$f" ] && [ -r "$f" ]; } || { echo unreadable; return; }
    bash -n "$f" 2>/dev/null || { echo unparseable; return; }
    [ "$( . "$f" >/dev/null 2>&1; echo __sourced__ )" = "__sourced__" ] \
        || { echo aborts; return; }
    echo ok
}
