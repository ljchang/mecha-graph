# personalized_knowledge_graph

A personal knowledge graph that turns your own data into context any agent can
use. Implements the **Personalized Context — Design** spec (Rev 2, 2026-08-02).

**The deliverable is not a database, it's a context pack**: every interface
returns a token-bounded, provenance-carrying, freshness-stamped slice.

```
        SOURCES                        CORE                       CONSUMERS
  ┌───────────────────────┐    ┌──────────────────────┐    ┌────────────────────┐
  │ Bee (API stream) ✅   │    │      pkg-core        │    │ Hermes      (MCP)  │
  │ Calendar (ICS) ✅     │───▶│  Rust library        │───▶│ Claude Code (MCP)  │
  │ Sessions ✅ · Slack ✅│    │  ingest · enrich ·   │    │ pkg CLI  ✅        │
  │ iMessage ✅ · mbox ✅ │    │  link · retrieve     │    │ DuckDB (analysis)  │
  └───────────────────────┘    │  SQLite (SQLCipher)  │    └────────────────────┘
                               │  + sqlite-vec + FTS5 │
                               └──────────────────────┘
```

**Mental model** (full version: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)):
data imports as **episodes** (append-only evidence, idempotent by source id);
linkers wire episodes to **nodes** (entities, identity via deterministic
identifiers → aliases → never-guess ambiguity) through **mentions**; and
**facts** (bi-temporal interpreted claims, each with episode provenance) are
either asserted directly by high-trust rungs or staged as candidates that
review promotes — *episodes are evidence, nodes are things, facts are
beliefs, the context pack is the product.*

## Layout

- `pkg-core/` — the library. Knows nothing about any agent (spec §2).
- `pkg-cli/` — the `pkg` binary (§9.2).
- `pkg-mcp/` — stdio MCP server exposing the five tools (§9.1):
  `kg_search`, `kg_entity`, `kg_timeline`, `kg_upsert`, `kg_related`.
- `eval/gold.jsonl` — the retrieval-quality ruler (§11), mined from your own
  corpus once you have one. `eval/synthetic/run.sh` is the self-contained
  version: it builds a throwaway graph from a fictional corpus and runs a
  24-query gold set against it — no personal data, no live store, works on
  a fresh clone.
- `scripts/nightly.sh` — the cron pipeline (03:30): source sync → linkers →
  GPU-gated embed/extract → MEMORY.md → health alerts.
- `docs/ARCHITECTURE.md` — the mental model: episodes → mentions/nodes →
  facts → context packs, the trust ladder, retrieval, time & privacy.
- `docs/INTEGRATIONS.md` — per-integration auth/config + the at-rest design.

## Quick start

```bash
cargo build --release

# 1. Register integrations (config: ~/pkg/config.toml; bee + sessions
#    self-register). See docs/INTEGRATIONS.md for auth details.
pkg source add bee --mode stream                  # API → DB, no plaintext files
pkg source add ics --url '<secret-ical-url>' --me you@example.edu
pkg source add slack --token xoxp-…
pkg source add mbox --path ~/Takeout/mail.mbox --me you@x.edu --retention capture_delete
pkg source list          # kind, enabled, auth state, last ok, items

# 2. Ingest everything enabled (cursored, idempotent):
pkg source sync

# 3. Re-run linkers over already-ingested episodes after new aliases land:
./target/release/pkg link --auto

# 4. Embeddings (ollama + nomic-embed-text, 768d):
./target/release/pkg embed

# Query — returns a context pack (JSON):
./target/release/pkg query "what did we discuss about the pilot data?"
./target/release/pkg query "when did I last meet with Nadia?"
./target/release/pkg entity "Nadia"
./target/release/pkg stats
./target/release/pkg eval            # against your graph and gold set
eval/synthetic/run.sh                # or the self-contained synthetic eval
```

DB lives at `~/pkg/graph.db` (override: `--db` or `PKG_DB`). Cheap ingestion is
separated from expensive enrichment (§5.4): `ingest` is fast and idempotent —
re-runs are no-ops via `UNIQUE(source, source_id)` + content hash; run `embed`
in nightly batches when the GPU is free.

## MCP wiring (§9.1)

Claude Code:

```bash
claude mcp add pkg -- ~/Github/personalized_knowledge_graph/target/release/pkg-mcp
```

Hermes (or any MCP client) — stdio transport:

```json
{ "mcpServers": { "pkg": { "command": "~/Github/personalized_knowledge_graph/target/release/pkg-mcp" } } }
```

Agent writes go through `kg_upsert` → `fact_candidate` staging with
`source='agent:<harness>'`; review with `pkg review`, `pkg accept/reject`.
Disambiguation answers (`kind='alias'`) land immediately as permanent aliases
(§11.2 — resolve at the point of use).

## Design notes / deviations from the spec

- **Stream-first ingestion** (settled 2026-08-02): sources with APIs (Bee,
  Slack, calendar URLs) stream straight into the DB — plaintext never touches
  disk. File-based sources (iMessage chat.db copies, mbox exports) use
  `retention = capture_delete`: full raw archived to `episode_raw` *inside
  the encrypted DB*, then the file is deleted after the archive row is
  verified. "Raw stays raw" (§2) is satisfied by the archive — `pkg raw
  <uid>` shows it, and re-enrichment/re-extraction read from it.
- **SQLCipher at rest** (§10): the raw key lives in `~/pkg/db.key` (0600),
  picked up automatically by every `pkg`/`pkg-mcp` open (`PKG_DB_KEY`/
  `PKG_DB_KEYFILE` override). Back the key up separately (password manager).
  DuckDB can't read SQLCipher, so analytics use an ephemeral snapshot:
  `pkg decrypt --out /tmp/analytics.db` (chmod 600) and attach that instead.
- **FTS5 arm first** (open decision §13): tantivy can be swapped in behind the
  same RRF interface; FlowMail keeps its tantivy index untouched.
- **ICS calendar source** instead of OAuth Google/MS Graph: headless-friendly;
  FlowMail's OAuth sync stays on macOS. RRULE masters are not expanded (v1).
- **FlowMail untouched**: pkg-core is a fresh extraction following FlowMail's
  patterns (same rusqlite/sqlite-vec stack, same migration runner). Pointing
  FlowMail at pkg-core (spec Phase 1 step 3) is a separate macOS-side change.
- **Bee/DM/SMS episodes are `private`** (§10): excluded from default
  retrieval; use `--private` / `include_private: true` to opt in per query.

## Privacy (§10)

`public < personal < private < secret`. Default retrieval excludes `private+`.
`pkg redact --episode <uid>` is the true-delete path: purges the episode, its
raw archive, mentions, embeddings, FTS rows, enrichment, and derived facts.

## Analytics (§8.4)

```bash
pkg decrypt --out /tmp/analytics.db   # ephemeral plaintext snapshot
```
```sql
INSTALL sqlite; LOAD sqlite;
ATTACH '/tmp/analytics.db' AS pkg (TYPE sqlite);
SELECT source, COUNT(*) FROM pkg.episode GROUP BY source;
```
