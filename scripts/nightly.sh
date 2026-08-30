#!/usr/bin/env bash
# Nightly pipeline (spec §5.4): cheap ingestion always; expensive enrichment
# (embed + Tier-7 extract) gated on the GPU being free so the graph never
# fights research work for the UMA box.
#
# Install:  crontab -e  →  30 3 * * *  $HOME/Github/personalized_knowledge_graph/scripts/nightly.sh
# Optional config in ~/.mecha-graph/nightly.env:
#   MECHA_GRAPH_ICS_URL=https://calendar.google.com/.../basic.ics   # secret iCal address
#   MECHA_GRAPH_SELF_EMAIL=you@example.edu
#   EXTRACT_LIMIT=100        # Tier-7 episodes per night
#   EXTRACT_MODEL=gemma4:e4b
#   EXTRACT_EXCLUDE="calendar.event"  # sources kept OUT of LLM extraction
#                            # (space-separated; empty string = extract all)
#   SUMMARIZE_LIMIT=30       # scope summaries refreshed per night (§4.5)
#   PRECHECK_AUTO_ACCEPT=0   # 0 disables auto-accept (durable predicates only)
#   LINK_PROPOSE=1           # re-enable the candidate-staging linker tiers
#   BEE_PULL_LIMIT=100       # re-enable the Bee suggested-facts pull
#   GPU_BUSY_THRESHOLD=30    # skip embed/extract above this % utilization
#   UTILITY_FLOOR=0.05       # retrieval-rate floor: demote + gate classes
#                            # under it (unset = report-only; set only after
#                            # fact_usage has weeks of data at the new rate)

set -uo pipefail
umask 077   # everything this script creates (logs, MEMORY.md) is private

# Cron's PATH lacks ~/.local/bin, where the bee CLI and node live — without
# this the bee source dies with "bee CLI not runnable" every night.
export PATH="$HOME/.local/bin:$PATH"

# The bee CLI keeps its API token in the Secret Service keyring, which it
# reaches over the D-Bus SESSION bus. cron inherits no session, so the
# lookup tries to autolaunch one and dies with "Cannot autolaunch D-Bus
# without X11 $DISPLAY" — which is what silently broke bee ingestion (the
# `bee stale` alert on 2026-08-13). The user's bus socket persists because
# lingering is enabled (`loginctl enable-linger`); point at it when it
# exists, and say so in the log when it doesn't, rather than failing
# nightly with a message about X11.
BUS_SOCK="/run/user/$(id -u)/bus"
if [ -S "$BUS_SOCK" ]; then
    export DBUS_SESSION_BUS_ADDRESS="unix:path=$BUS_SOCK"
else
    BEE_BUS_MISSING=1
fi

# $HOME/pkg was this store's location before the pkg -> mecha-graph rename.
# It survived here after the live copy moved, and on 2026-08-17 a run from this
# repo created a fresh, EMPTY graph.db there — schema initialised, zero rows —
# which then sat beside the real 186 MB store looking like a second database
# somebody might need to migrate. Nothing was lost; the risk was the opposite,
# that the empty one gets mistaken for real.
MECHA_GRAPH_DIR="${MECHA_GRAPH_DIR:-$HOME/.mecha-graph}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG="$REPO_DIR/target/release/mecha-graph"
LOG_DIR="$MECHA_GRAPH_DIR/logs"
LOG="$LOG_DIR/nightly-$(date +%Y%m%d).log"

mkdir -p "$LOG_DIR"
# Keep 30 days of logs.
find "$LOG_DIR" -name 'nightly-*.log' -mtime +30 -delete 2>/dev/null

[ -f "$MECHA_GRAPH_DIR/nightly.env" ] && . "$MECHA_GRAPH_DIR/nightly.env"
EXTRACT_LIMIT="${EXTRACT_LIMIT:-100}"
EXTRACT_MODEL="${EXTRACT_MODEL:-gemma4:e4b}"
# Calendar is 65% of the corpus and its bodies are titles + attendee lists
# the deterministic tiers already extracted; LLM-extracting them was the
# single largest manufacturer of review-queue trivia. Opt back in with
# EXTRACT_EXCLUDE="" in nightly.env — which is why this is `-`, not `:-`:
# the colon form treats empty as unset and would silently re-apply the
# default, making the documented opt-out a no-op.
EXTRACT_EXCLUDE="${EXTRACT_EXCLUDE-calendar.event}"
SUMMARIZE_LIMIT="${SUMMARIZE_LIMIT:-30}"
PRECHECK_AUTO_ACCEPT="${PRECHECK_AUTO_ACCEPT:-1}"
GPU_BUSY_THRESHOLD="${GPU_BUSY_THRESHOLD:-30}"

log() { echo "[$(date '+%F %T')] $*" >>"$LOG"; }
run() { log "\$ $*"; "$@" >>"$LOG" 2>&1 || log "FAILED (exit $?): $*"; }

log "=== nightly start ==="
if [ -n "${BEE_BUS_MISSING:-}" ]; then
    log "WARNING: no D-Bus session socket at $BUS_SOCK — the bee source will \
fail to read its keyring token. Fix: loginctl enable-linger $USER (and log in \
once so the keyring is unlocked)."
fi

# ── Cheap ingestion (always) ─────────────────────────────────────────────────
# All configured integrations (~/.mecha-graph/config.toml): bee (streamed), sessions,
# calendar, slack, imessage, mbox — whatever is enabled.
# Manage with `mecha-graph source list|add|test`.
run "$PKG" source sync

# ── Linkers (cheap, CPU-only) ────────────────────────────────────────────────
# Deterministic tiers only (alias, temporal, NPMI): the mention substrate
# decay and the rollups read. The candidate-staging tiers (kNN, structural,
# rules) ran at 4–14% human accept for a month with nothing consuming that
# rate — ~190 unwanted proposals a night. They are opt-in per run now:
# LINK_PROPOSE=1 in nightly.env turns them back on.
LINK_PROPOSE="${LINK_PROPOSE:-0}"
if [ "$LINK_PROPOSE" = "1" ]; then
    run "$PKG" link --propose
else
    run "$PKG" link
fi

# Phantom repair, BEFORE decay and AFTER link, and the order is the whole
# point. `link` learns aliases — that an address belongs to a person — and
# consolidating mentions onto the person is what strands the co-occurrence
# belief between a person and their own email. Those beliefs were never true;
# they are artifacts of pre-alias over-linking, so they take a system-time
# retraction rather than decay's valid-time close.
#
# It was written as a one-shot repair, but the condition recurs every time
# alias-learning improves, and nothing ran it: the alarm count went 0 → 48 →
# 193 across three nights while decay dutifully refused to touch any of them.
# Running it here keeps decay's alarm list meaning "something is genuinely
# wrong" instead of becoming a standing pile nobody reads. The 2026-08-15
# repair cleared 140 and took the alarms to 46.
#
# It never touches a user-verified belief, and it leaves partial collapses
# alone — those are ambiguous between drift and data loss, which is exactly
# what decay's alarm is for.
run "$PKG" invalidate-phantoms

# Decay sweep (§11.5): re-derive co-occurrence beliefs against the mentions
# the linkers just rebuilt, close the collapsed ones (valid time only) and
# refresh drifted numbers. Capped per run, so a backlog drains gradually and
# visibly via `mecha-graph stats`. MUST follow link: it reads the fresh mention table.
run "$PKG" decay

# Bee suggested-facts two-way sync. The PULL is retired by default: the
# lane produced six live facts ever out of ~1,700 processed, at a 38%
# human accept rate, with a known misattribution bias toward the owner —
# the queue's single worst cost-per-kept-fact. The PUSH always runs, so
# verdicts on anything already staged still clean Bee's app. Opt the pull
# back in with BEE_PULL_LIMIT in nightly.env. The archive of everything
# the lane ever staged: ~/.mecha-graph/archive/bee-facts-2026-08-28.jsonl.gz.
BEE_PULL_LIMIT="${BEE_PULL_LIMIT:-0}"
run "$PKG" bee-facts --pull-limit "$BEE_PULL_LIMIT"

# D3 corrections backfill: kg_upsert processes meta.corrections inline;
# this drains anything that arrived another way or failed mid-flight.
run "$PKG" corrections

# ── Expensive enrichment, gated on GPU idleness (§5.4) ───────────────────────
#
# The gate waits before it gives up. It used to be one instantaneous sample:
# if the GPU happened to be busy at the moment cron fired, embed, extract,
# precheck and summarize were all skipped until the next night. That was
# tolerable at 03:30, when the box is reliably asleep. The cron moved to 01:30
# on 2026-08-15 (qwen3.6 extraction runs 45s/episode, so 400 episodes need ~5h
# to finish before the morning briefing), and 01:30 is a time somebody is
# plausibly still working — turning a one-shot check into a coin flip on
# whether the graph learns anything that night.
#
# So: poll every 5 minutes for up to GPU_WAIT_MINUTES (default 30) and start
# when it frees. The wait is bounded rather than open-ended because the point
# of the whole schedule is to be done before the briefing — 30 minutes is what
# the 01:30 slot can spare and still land by ~07:00 in the worst case. Waiting
# is logged, so a night that started late says so rather than looking slow.
GPU_WAIT_MINUTES="${GPU_WAIT_MINUTES:-30}"
gpu_util() {
    local u
    u="$(nvidia-smi --query-gpu=utilization.gpu --format=csv,noheader,nounits 2>/dev/null | head -1 | tr -dc '0-9')"
    # No nvidia-smi, or unreadable: treat as idle, exactly as the old
    # `${GPU_UTIL:-0}` did. Failing open here is deliberate — a box without a
    # usable GPU query should still do its nightly work.
    echo "${u:-0}"
}

GPU_UTIL="$(gpu_util)"
waited=0
while [ "$GPU_UTIL" -gt "$GPU_BUSY_THRESHOLD" ] && [ "$waited" -lt "$GPU_WAIT_MINUTES" ]; do
    log "GPU busy (${GPU_UTIL}% > ${GPU_BUSY_THRESHOLD}%): waiting 5m (${waited}/${GPU_WAIT_MINUTES}m elapsed)"
    sleep 300
    waited=$((waited + 5))
    GPU_UTIL="$(gpu_util)"
done
[ "$waited" -gt 0 ] && [ "$GPU_UTIL" -le "$GPU_BUSY_THRESHOLD" ] && \
    log "GPU free after ${waited}m wait (${GPU_UTIL}%) — proceeding"

if [ "$GPU_UTIL" -le "$GPU_BUSY_THRESHOLD" ]; then
    run "$PKG" embed
    EXTRACT_ARGS=(--limit "$EXTRACT_LIMIT" --model "$EXTRACT_MODEL")
    for src in $EXTRACT_EXCLUDE; do EXTRACT_ARGS+=(--exclude-source "$src"); done
    run "$PKG" extract "${EXTRACT_ARGS[@]}"
    # Auto-triage the fresh candidates: duplicates die, contradictions get
    # flagged, and (opt-in) clean novel facts accept themselves.
    if [ "$PRECHECK_AUTO_ACCEPT" = "1" ]; then
        run "$PKG" precheck --auto-accept
    else
        run "$PKG" precheck
    fi
    run "$PKG" summarize --limit "$SUMMARIZE_LIMIT" --model "$EXTRACT_MODEL"
else
    log "GPU still busy (${GPU_UTIL}%) after ${GPU_WAIT_MINUTES}m: skipping embed/extract tonight"
fi

# ── Entity maintenance (cheap, CPU-only, no model) ───────────────────────────
# The entity-layer counterpart of extract+precheck: six detectors file
# proposals for a person to decide. It runs OUTSIDE the GPU gate because it is
# pure SQL — 1.5s over 14k nodes — and a night when the GPU stayed busy is
# exactly a night when the cheap checks should still happen.
#
# It repairs nothing. The repair direction is usually not derivable from the
# data, and a decided proposal is never re-filed, so this is idempotent and
# silent once the queue is drained.
run "$PKG" audit

# ── The utility loop (review-on-use §3, CPU-only) ────────────────────────────
# One loud line per night: which classes the query stream is voting against,
# what the precision gate blocks from extraction, and — once UTILITY_FLOOR is
# set in nightly.env (leave unset until fact_usage has weeks of tenure) —
# applied ladder demotions.
if [ -n "${UTILITY_FLOOR:-}" ]; then
    run "$PKG" utility --floor "$UTILITY_FLOOR" --apply
else
    run "$PKG" utility
fi

# ── Boot context + health ────────────────────────────────────────────────────
run "$PKG" memory-md --out "$MECHA_GRAPH_DIR/MEMORY.md"
"$PKG" stats >>"$LOG" 2>&1

# Surface §11.4 alert signals into the log header for quick scanning.
STALE="$("$PKG" stats 2>/dev/null | python3 -c "
import json,sys
try: h=json.load(sys.stdin)
except: sys.exit(0)
alerts=[]
if h['merge_queue_depth']>10: alerts.append(f\"merge queue {h['merge_queue_depth']}\")
if h['isolated_pct']>25: alerts.append(f\"isolated {h['isolated_pct']:.0f}%\")
if h['live_contradictions']>0: alerts.append(f\"{h['live_contradictions']} contradictions\")
for s in h['ingest_state']:
    if s['stale']: alerts.append(f\"{s['source']} stale\")
print('; '.join(alerts))
")"
# A precheck run whose embedding died mid-way prints its blindness marker
# into this very log; surface it as an alert rather than leaving zeros
# that read like a clean queue (grep target kept in step with main.rs).
if grep -q "SEMANTIC TIERS SKIPPED" "$LOG"; then
    STALE="${STALE:+$STALE; }precheck ran blind: embedding failed mid-run"
fi

if [ -n "$STALE" ]; then
    log "ALERTS: $STALE"
else
    log "health: no alerts"
fi

log "=== nightly done ==="
