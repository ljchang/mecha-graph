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
# The probe runs in a CLEAN environment (HOME, one fixed PATH, and
# MECHA_GRAPH_DIR as the file's own directory, under `set -u`), never the
# caller's: the two nightlies define different variables — and build
# different PATHs — before they ask, so probing in their live state would
# let one file that names a variable only one of them sets get two answers.
# A shared rule must also be a shared answer, for the verdict AND the values.
#
# That PATH leads with the user bin dirs the nightlies prepend — the union of
# the two: `~/.local/bin` (both; the bee CLI and node live there) and
# `~/.cargo/bin` (nightly-mecha.sh; `mecha` lives there). A bare system PATH
# would be shared but unfaithful, and a line like
# `command -v bee && PRECHECK_TRIAGE=0` would resolve "not found" in both
# halves — agreement on the OPEN answer. HOME and the PATH are fixed when this
# file is sourced, so nothing a caller sources afterwards can move them.
NIGHTLY_ENV_HOME="$HOME"
NIGHTLY_ENV_PATH="$HOME/.local/bin:$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin"
nightly_env_clean() {
    env -i HOME="$NIGHTLY_ENV_HOME" PATH="$NIGHTLY_ENV_PATH" MECHA_GRAPH_DIR="$(dirname "$1")" \
        bash -u -c "$2" _ "$1" "${3:-}"
}
nightly_env_status() {
    local f=$1
    # A dangling symlink fails `-e`, but it is a file the operator believes
    # is there — `absent` would take the defaults silently, and the default
    # runs the lane that makes permanent rejects.
    [ -e "$f" ] || [ -L "$f" ] || { echo absent; return; }
    { [ -f "$f" ] && [ -r "$f" ]; } || { echo unreadable; return; }
    # Same clean environment as the source probe below, so the syntax check
    # and the abort check are one interpreter — a newer bash on the caller's
    # PATH parsing what the probe's bash cannot would read as `aborts` and
    # drop the whole file.
    nightly_env_clean "$f" 'bash -n "$1"' 2>/dev/null || { echo unparseable; return; }
    [ "$(nightly_env_clean "$f" '. "$1" >/dev/null 2>&1; echo __sourced__')" = "__sourced__" ] \
        || { echo aborts; return; }
    echo ok
}

# The value NAME takes in FILE, read in the same clean environment the
# status probe used; empty when the file leaves it unset.
nightly_env_value() {
    nightly_env_clean "$1" '. "$1" >/dev/null 2>&1; printf "%s" "${!2:-}"' "$2"
}

# A precheck toggle, resolved the one way both nightlies use:
#   STATUS ok      → the file's value (clean env), else PROCESS, else 1
#   STATUS absent  → PROCESS, else 1
#   anything else  → 0 (fail closed)
# PROCESS is the caller's value from before it sourced anything, so a file
# that sets the toggle conditionally on the caller's own environment cannot
# make the halves disagree.
nightly_env_toggle() {
    local file=$1 status=$2 name=$3 process=${4:-} from_file=""
    case "$status" in
        ok) from_file="$(nightly_env_value "$file" "$name")" ;;
        absent) ;;
        *) printf '0'; return ;;
    esac
    printf '%s' "${from_file:-${process:-1}}"
}
