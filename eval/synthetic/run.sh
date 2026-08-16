#!/usr/bin/env bash
# Build a throwaway graph from the synthetic corpus and run the gold set
# against it. Self-contained: a fresh DB under a fixed throwaway key, so it
# neither reads nor risks the real store. Requires ollama (embeddings) at
# 127.0.0.1:11434, same as the eval harness itself.
#
# Usage: eval/synthetic/run.sh  (from the repo root or anywhere)

set -euo pipefail
cd "$(dirname "$0")/../.."

if ! curl -sf http://127.0.0.1:11434/api/tags >/dev/null 2>&1; then
  echo "ollama is not reachable at 127.0.0.1:11434 — the eval needs embeddings" >&2
  exit 1
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export MECHA_GRAPH_DB="$tmp/graph.db"
# The config too, or `source add` writes synthetic entries into the real
# ~/.mecha-graph/config.toml — learned by doing exactly that.
export MECHA_GRAPH_CONFIG="$tmp/config.toml"
# A throwaway key for a throwaway DB: the corpus is synthetic and public by
# design, so secrecy would protect nothing.
export MECHA_GRAPH_DB_KEY="synthetic-eval"

cargo build --release -p mecha-graph-cli >/dev/null
bin="./target/release/mecha-graph"

# `add` registers; `sync` ingests. Both are needed — an added source with no
# sync is an empty graph, and every query misses.
"$bin" source add mbox --path eval/synthetic/corpus/mail.mbox \
    --me ada.lovelace@example.edu
"$bin" source add ics --path eval/synthetic/corpus/calendar.ics \
    --me ada.lovelace@example.edu
"$bin" source sync mbox
"$bin" source sync ics

"$bin" eval --gold eval/synthetic/gold.jsonl
