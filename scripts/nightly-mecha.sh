#!/usr/bin/env bash
# The mecha-side half of the nightly: everything that needs the CHAT model on
# llama-server, run after pkg's own sweep has finished with ollama.
#
#   1. vet    — judge pending candidates in the auto-accept classes
#   2. precheck --auto-accept — bank the verdicts vet just filed
#   3. gossip — probe one or more entities, rotating
#
# Install:  crontab -e  →  0 8 * * *  .../scripts/nightly-mecha.sh
# (was gossip-nightly.sh; renamed 2026-08-16 when vet moved in.)
#
# ── Why vet is here and not in nightly.sh ────────────────────────────────────
#
# Because it is the missing link in the auto-accept lane, and the 2026-08-16
# run proved it: extraction added 397 candidates and precheck auto-accepted
# ZERO, because the durable lane requires a per-candidate vet witness and
# nothing had filed one. The queue went 3800 → 4275 in a night. vet ran only
# when a human typed it, so the lane ran on fuel nobody was pouring.
#
# It lives here rather than in pkg's nightly for the same reason gossip does:
# vet is a mecha agent run needing llama-server (8080), while pkg's extraction
# needs ollama, and on unified memory those two contend. This script is the
# seam between the repos — pkg's nightly stays pure pkg.
#
# Order matters: vet FILES verdicts, precheck CONSUMES them. Running precheck
# first would bank nothing, which is exactly last night's result.
#
# ── Why gossip rotates, and why that needed fixing ───────────────────────────
#
# The Selector ranks by demand × slot-gaps × λ-staleness, where demand is
# `retrieval_touch` — "bumped for every item that enters a returned pack".
# Gossip reads the graph about its target, so probing an entity RAISES that
# entity's demand: one target went touches=2 (score 2.20) to touches=28
# (score 6.73) after one probe, and won the next night by a wider margin. A
# self-reinforcing loop, on a pool of only nine viable targets.
#
# The clean fix is upstream — gossip's own reads should not count as demand —
# but that is a change to pkg's touch accounting with a schema question
# attached. This is the cheap correct one: skip anything probed in the last
# GOSSIP_COOLDOWN_DAYS, and it fails open (no history ⇒ nothing excluded).
#
# ── Why the cooldown is keyed on the ATTEMPT, not the output ─────────────────
#
# It used to read the gossip JSONL, on the reasoning that "the JSONL already
# records entity and timestamp" — which is true of every probe that produced
# a row, and silently false of every probe that did not. Gossip needs two
# independent sources ("one witness cannot gossip"); an entity with a single
# source is refused, exits 0, and writes nothing. So it never entered the
# cooldown, while the Selector went on ranking it highly for exactly the
# missing slots gossip is structurally unable to fill.
#
# One entity was therefore re-probed every night from 2026-08-28 to
# 2026-09-01 (and on 2026-08-20), burning one of three nightly slots forever:
# the log said `probed:` three times a night and the audit said
# `gossip audit across 2 run(s)`, which is where the gap was visible.
#
# The ledger below records the ATTEMPT, so a refusal ages exactly like a
# success. Same shape as the Bee push failure fixed alongside it: an outcome
# that leaves no record cannot be aged, so it repeats forever.
set -uo pipefail
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG="${MECHA_GRAPH_BIN:-$REPO_DIR/target/release/mecha-graph}"
MECHA="${MECHA_BIN:-$HOME/.cargo/bin/mecha}"
LOG_DIR="$HOME/.mecha-graph/logs"
LOG="$LOG_DIR/mecha-nightly-$(date +%Y%m%d).log"
GOSSIP_OUT="$LOG_DIR/gossip-$(date +%Y%m%d).jsonl"
# Every probe attempt, whatever it produced. Read by the cooldown.
PROBE_LEDGER="$LOG_DIR/probed-$(date +%Y%m%d).jsonl"
VET_OUT="$LOG_DIR/vet-durable-$(date +%Y%m%d).jsonl"

ENTITIES="${GOSSIP_ENTITIES:-3}"
COOLDOWN="${GOSSIP_COOLDOWN_DAYS:-7}"
VET_LIMIT="${VET_LIMIT:-40}"
WORKSPACE="${GOSSIP_WORKSPACE:-$LOG_DIR}"

# The classes the auto-accept lane can actually admit (DURABLE_CLASSES in
# precheck.rs, llm-proposed). Vetting anything else files opinions that no
# lane consumes — useful for a human, but not for draining the queue. Keep
# this list in step with precheck.rs or vet spends the night on classes whose
# verdicts change nothing.
DURABLE_PREDICATES=(uses works_on works_at authored collaborates_with
                    member_of located_in related_to contains discussed_during)

umask 077
mkdir -p "$LOG_DIR"
log() { echo "[$(date '+%F %T')] $*" >>"$LOG"; }

log "=== mecha nightly start ==="
for bin in "$PKG" "$MECHA"; do
    [ -x "$bin" ] || { log "FATAL: $bin not executable — nothing ran"; exit 1; }
done
if ! curl -sf -m 5 http://127.0.0.1:8080/health >/dev/null 2>&1; then
    log "llama-server on :8080 not answering — skipping tonight"
    exit 0
fi

# ── 1. vet ───────────────────────────────────────────────────────────────────
# kg_pending's `unjudged_by` means a rerun EXTENDS coverage rather than
# re-judging the same oldest N, so a class with nothing new costs one cheap
# call and moves on.
for pred in "${DURABLE_PREDICATES[@]}"; do
    if timeout 30m "$MECHA" vet --proposer llm --predicate "$pred" \
        --limit "$VET_LIMIT" --record --out "$VET_OUT" >>"$LOG" 2>&1; then
        log "vetted: llm/$pred"
    else
        log "vet FAILED (exit $?): llm/$pred — continuing"
    fi
done

# ── 2. bank the verdicts ─────────────────────────────────────────────────────
# pkg's nightly already ran precheck hours ago, before these verdicts existed.
# This second pass is what turns them into accepts.
run_precheck() { "$PKG" precheck --auto-accept >>"$LOG" 2>&1 || log "precheck FAILED"; }
log "precheck (banking tonight's verdicts)"
run_precheck

# ── 3. gossip, rotating ──────────────────────────────────────────────────────
# One line per probe attempt. `entity` matches the gossip output's own key so
# the cooldown reader treats both files identically.
record_attempt() {
    python3 - "$PROBE_LEDGER" "$1" "${2:-}" <<'LEDGER' 2>>"$LOG"
import json, sys, datetime
path, entity = sys.argv[1], sys.argv[2]
node_id = sys.argv[3] if len(sys.argv) > 3 else ""
# **`node_id` beside the name, and it is the durable half.** `display` is
# derived — `best_label` prefers a human alias over an email-shaped name, so
# promoting an alias or renaming a node changes it, and a cooldown keyed on
# the label alone silently forgets that node was probed yesterday. Names are
# still written and still matched, because every row from before this change
# has only a name.
row = {"entity": entity,
       "node_id": node_id,
       "at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
with open(path, "a") as fh:
    fh.write(json.dumps(row) + "\n")
LEDGER
}

# **Failing open is right for ABSENT history and wrong for an unreadable
# one.** No ledger yet means nothing to exclude, which is correct on a fresh
# install. An unreadable log dir means the cooldown silently excludes nobody
# and every entity looks fresh — the repeat this branch exists to stop, in
# the one state where nothing would notice. Status is read below.
RECENT="$(python3 - "$LOG_DIR" "$COOLDOWN" <<'PY' 2>>"$LOG"
import glob, json, os, sys, time, datetime
log_dir, days = sys.argv[1], int(sys.argv[2])
cutoff = time.time() - days * 86400
# **glob() cannot tell you it failed.** On a missing or unreadable directory
# it returns [] exactly as it does for an empty one, so "no history" and
# "cannot read the history" arrive identically — and the second silently
# excludes nobody, making every entity look fresh. Ask the directory
# directly, and exit non-zero so the caller can refuse rather than probe
# everything.
if not os.path.isdir(log_dir) or not os.access(log_dir, os.R_OK | os.X_OK):
    print(f"cooldown: {log_dir} is not a readable directory", file=sys.stderr)
    sys.exit(1)
# entity -> most recent probe, as an epoch. A MAP, not a set: when every
# ranked target is on cooldown the night must still probe something, and
# "least recently probed" is the only sensible order to fall back to.
seen = {}
# `probed-*` is the attempt ledger (every probe, refusals included);
# `gossip-*` is the output, kept so history written before the ledger
# existed still counts toward the cooldown.
paths = glob.glob(os.path.join(log_dir, "probed-*.jsonl"))
paths += glob.glob(os.path.join(log_dir, "gossip-*.jsonl"))
for path in paths:
    mtime = os.path.getmtime(path)
    for line in open(path):
        line = line.strip()
        if not line:
            continue
        try:
            row = json.loads(line)
        except Exception:
            continue          # a truncated line must not cost the rotation
        ent = row.get("entity")
        if not ent:
            continue
        # Key on the id where a row has one, else the name. Both are emitted
        # so a rename does not orphan the history, and rows written before
        # ids were recorded still match by name.
        key = row.get("node_id") or ent
        # The row's own stamp where it has one, else the file's mtime. The
        # ledger writes `at` per row; the older gossip output does not.
        when = mtime
        at = row.get("at")
        if at:
            try:
                when = datetime.datetime.fromisoformat(at).timestamp()
            except Exception:
                pass
        seen[key] = max(seen.get(key, 0.0), when)
        if key != ent:
            seen[ent] = max(seen.get(ent, 0.0), when)
for ent, when in sorted(seen.items()):
    print("%s\t%d\t%d" % (ent, when, 1 if when >= cutoff else 0))
PY
)"
RECENT_STATUS=$?
if [ "$RECENT_STATUS" -ne 0 ]; then
    log "cooldown reader FAILED (exit $RECENT_STATUS) — refusing to gossip rather than \
probing everything as if nothing were recent; see the stderr above in this log."
    log "=== mecha nightly done (cooldown unreadable) ==="
    exit 1
fi
[ -n "$RECENT" ] && log "cooldown (${COOLDOWN}d) excludes: $(echo "$RECENT" | awk -F'\t' '$3==1{printf "%s ", $1}')"

# Ask for more than needed, then filter — the excluded ones are usually the
# top-ranked, being the ones a previous probe inflated.
# `python3 -c`, NOT `python3 - <<'PY'`: the JSON arrives on stdin, and a
# heredoc would claim stdin for the program text instead — the filter then
# reads nothing, silently returns no targets, and the night looks like "all
# entities are on cooldown". Cost one debugging round on 2026-08-16.
# `--min-sources 2` is gossip's own precondition: two independent sources or
# it refuses. Without it the Selector keeps offering nodes gossip cannot read,
# and since a refusal fills no slots they score just as highly the next night.
TARGETS="$("$PKG" probe-targets --limit 25 --min-sources 2 --json 2>>"$LOG" | python3 -c '
import json, sys
# entity -> (last_probed_epoch, on_cooldown)
hist = {}
for line in sys.argv[1].splitlines():
    parts = line.split("\t")
    if len(parts) == 3:
        hist[parts[0]] = (int(parts[1]), parts[2] == "1")
want = int(sys.argv[2])
try:
    rows = json.load(sys.stdin)
except Exception:
    rows = []
# name and id together: the id is what the ledger keys on, the name is
# what `gossip --entity` takes.
pairs = [((r.get("display") or r.get("name")), r.get("node_id") or "") for r in rows]
pairs = [(n, i) for n, i in pairs if n]
names = [n for n, _ in pairs]
ids = dict(pairs)

fresh = [n for n in names if not hist.get(n, (0, False))[1]]
if len(fresh) >= want:
    print("\n".join(f"{n}\t{ids.get(n,'')}" for n in fresh[:want]))
else:
    # **The pool can be smaller than the cooldown holds.** GOSSIP_ENTITIES=3
    # a night over GOSSIP_COOLDOWN_DAYS=7 ages 21 distinct entities, and the
    # ranked pool under `--min-sources 2` is 20 — so at steady state every
    # target is on cooldown and the night probes NOTHING. That is a
    # composition of the two halves of this branch: excluding un-gossipable
    # entities from the pool is right, and ageing every attempt is right,
    # but together they drain the pool monotonically. Skipping the night
    # would trade a wasted slot for no slot at all.
    #
    # So the cooldown is a PREFERENCE, not a gate: take the fresh ones
    # first, then fill from the least-recently-probed. Rotation is what the
    # cooldown was ever for, and oldest-first is rotation even when nothing
    # is strictly fresh.
    stale = [n for n in names if n not in fresh]
    stale.sort(key=lambda n: hist.get(n, (0, False))[0])
    print("\n".join(f"{n}\t{ids.get(n,'')}" for n in (fresh + stale)[:want]))
' "$RECENT" "$ENTITIES" 2>>"$LOG")"
# **A Selector that could not RUN is not an empty queue.** `set -o pipefail`
# is on, so this is the pipeline's status: a locked database, an unreadable
# keyfile or a mid-flight migration all make `probe-targets` exit non-zero,
# the filter reads nothing, and `TARGETS` comes back empty — identical to a
# night where every target was genuinely on cooldown. Reported as the
# cooldown, gossip stops silently and the log names the one cause an
# operator will not look behind, because it is the cause that really does
# produce that line.
#
# Unknown is never clean, and this file has already paid a debugging round
# for the same conflation one comment up (the heredoc that ate stdin). That
# instance is fixed; the class was not.
TARGETS_STATUS=$?

if [ "$TARGETS_STATUS" -ne 0 ]; then
    log "probe-targets FAILED (exit $TARGETS_STATUS) — NOT a cooldown; skipping gossip. \
Check the graph store is readable and not mid-migration; see the stderr above in this log."
elif [ -z "$TARGETS" ]; then
    log "no fresh probe targets (all ${COOLDOWN}d-recent or none ranked) — skipping gossip"
else
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        entity="${line%%$'\t'*}"
        node_id="${line#*$'\t'}"
        [ "$node_id" = "$line" ] && node_id=""
        log "probing: $entity"
        # Written BEFORE the probe, not after: a run killed by the 30m
        # timeout, or by the box going down mid-probe, has still consumed the
        # slot and must still age. Recording on success only is the bug this
        # ledger exists to fix, one level down.
        record_attempt "$entity" "$node_id"
        # `< /dev/null`: the loop is fed by a here-string, so without it
        # gossip inherits the REMAINING entity lines as its own stdin and can
        # eat them — the loop then probes one target and reports the rest as
        # never selected. Same class as the `python3 -c` note above, which
        # cost a debugging round when a heredoc claimed stdin.
        if timeout 30m "$MECHA" gossip --entity "$entity" --yes \
            --workspace "$WORKSPACE" --out "$GOSSIP_OUT" >>"$LOG" 2>&1 </dev/null; then
            log "probed: $entity"
        else
            log "gossip FAILED (exit $?): $entity — continuing"
        fi
    done <<<"$TARGETS"
fi

# ── summary, because a transcript nobody reads surfaces nothing twice ────────
[ -f "$GOSSIP_OUT" ] && python3 - "$GOSSIP_OUT" >>"$LOG" 2>&1 <<'PY'
import json, sys
from collections import Counter
# Per-line, guarded — the cooldown reader over these same files already
# skips a torn line rather than dying on it, and a summary that raises takes
# the whole night's log with it. A nightly appends while this may run.
rows = []
for l in open(sys.argv[1]):
    l = l.strip()
    if not l:
        continue
    try:
        rows.append(json.loads(l))
    except Exception:
        continue
c = Counter(a["verdict"] for r in rows for a in r.get("audit", []))
print(f"  gossip audit across {len(rows)} run(s): {dict(c)}")
for r in rows:
    for a in r.get("audit", []):
        if a["verdict"] in ("contradicted", "unsupported"):
            claim = a.get("claim") or a.get("statement", "")
            print(f"  ⚑ {r['entity']}: {str(claim)[:110]}")
PY

log "=== mecha nightly done ==="
