# mecha-graph

A personal knowledge graph that turns your own data — mail, calendar, notes,
messages — into context any agent can use, served over
[MCP](https://modelcontextprotocol.io) to any client. Built as a sibling of
[mecha](https://github.com/ljchang/mecha), usable without it.

**The deliverable is not a database, it's a context pack**: every interface
returns a token-bounded, provenance-carrying, freshness-stamped slice.

```
        SOURCES                          CORE                         CONSUMERS
  ┌───────────────────────┐    ┌───────────────────────┐    ┌─────────────────────┐
  │ calendar (ICS)        │    │   mecha-graph-core    │    │ mecha        (MCP)  │
  │ mbox mail exports     │───▶│   Rust library        │───▶│ Claude Code  (MCP)  │
  │ Slack · iMessage      │    │   ingest · enrich ·   │    │ any MCP client      │
  │ notes · wearables     │    │   link · retrieve     │    │ mecha-graph CLI     │
  └───────────────────────┘    │   SQLite (SQLCipher)  │    │ DuckDB (analytics)  │
                               │   + sqlite-vec + FTS5 │    └─────────────────────┘
                               └───────────────────────┘
```

**Mental model** (full version: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)):
data imports as **episodes** (append-only evidence, idempotent by source id);
linkers wire episodes to **nodes** (entities, identity via deterministic
identifiers → aliases → never-guess ambiguity) through **mentions**; and
**facts** (bi-temporal interpreted claims, each with episode provenance) are
either asserted directly by high-trust sources or staged as candidates that
review promotes — *episodes are evidence, nodes are things, facts are
beliefs, the context pack is the product.*

## Install

```bash
cargo install mecha-graph          # the CLI
cargo install mecha-graph-mcp      # the MCP server
```

Or from a checkout: `cargo build --release` (binaries land in
`target/release/`). Embeddings need [ollama](https://ollama.com) with
`nomic-embed-text` on localhost; everything else is self-contained.

## Try it with no data at all

```bash
eval/synthetic/run.sh
```

builds a throwaway graph from a fictional corpus (a twelve-message mailbox
and a small calendar) and grades 24 retrieval queries against it — no
personal data, no live store, works on a fresh clone. It doubles as the
retrieval-quality ruler: `eval/gold.jsonl` is the same format, mined from
your own corpus once you have one.

## Quick start with your data

```bash
# 1. Register integrations (config: ~/.mecha-graph/config.toml).
#    See docs/INTEGRATIONS.md for per-source auth.
mecha-graph source add ics --url '<secret-ical-url>' --me you@example.edu
mecha-graph source add mbox --path ~/Takeout/mail.mbox --me you@example.edu --retention capture_delete
mecha-graph source add slack --token xoxp-…
mecha-graph source list           # kind, enabled, auth state, last ok, items

# 2. Ingest everything enabled (cursored, idempotent — re-runs are no-ops):
mecha-graph source sync

# 3. Link entities, then embed when the GPU is free:
mecha-graph link --auto
mecha-graph embed

# Query — returns a context pack (JSON):
mecha-graph query "what did we discuss about the pilot data?"
mecha-graph entity "Nadia"
mecha-graph stats
```

The store lives at `~/.mecha-graph/graph.db` (override with `--db` or
`MECHA_GRAPH_DB`). Cheap ingestion is deliberately separated from expensive
enrichment: `sync` is fast and idempotent; run `embed` and `extract` in
nightly batches (`scripts/nightly.sh` is the shipped shape of that).

Agent-facing writes go through `kg_upsert` → candidate staging; **nothing an
agent writes becomes a belief until you review it**: `mecha-graph review`,
`accept`, `reject`, and `precheck` to auto-triage duplicates.

## MCP wiring

The server speaks stdio and exposes eleven tools: `kg_search`, `kg_entity`,
`kg_timeline`, `kg_related`, `kg_upsert`, `kg_verify`, `kg_pending`,
`kg_verdict`, and a small task family (`kg_task_list` / `create` / `update`).

**mecha** (`~/.mecha/config.toml`) — the tools carry their own `kg_`
namespace, so skip the server prefix, and mark the graph untrusted so
reading it arms the trifecta interlock:

```toml
[[mcp]]
name = "graph"
command = "mecha-graph-mcp"
prefix_tools = false

[mcp.capabilities]
untrusted_input = true
```

**Claude Code**:

```bash
claude mcp add graph -- mecha-graph-mcp
```

**Any other MCP client** — stdio transport:

```json
{ "mcpServers": { "graph": { "command": "mecha-graph-mcp" } } }
```

## Your data stays yours

- **Encrypted at rest.** The store is SQLCipher; the raw key lives in
  `~/.mecha-graph/db.key` (0600), picked up automatically by every open
  (`MECHA_GRAPH_DB_KEY` / `MECHA_GRAPH_DB_KEYFILE` override). Back the key
  up separately — a password manager, not the same disk.
- **Stream-first ingestion.** Sources with APIs stream straight into the
  encrypted DB — plaintext never touches disk. File-based sources (mbox,
  chat.db copies) can use `--retention capture_delete`: the raw is archived
  *inside* the encrypted DB, then the file is deleted once the archive row
  verifies. `mecha-graph raw <uid>` shows the archive; re-enrichment reads
  from it.
- **A sensitivity ladder.** `public < personal < private < secret`; default
  retrieval excludes `private` and above — messages and wearable transcripts
  land as `private`. Opt in per query with `--private` /
  `include_private: true`.
- **True delete.** `mecha-graph redact --episode <uid>` purges the episode,
  its raw archive, mentions, embeddings, FTS rows, enrichment, and derived
  facts; `tombstone` keeps re-ingest from resurrecting it.
- **Local by construction.** Nothing reaches the network except the
  integrations you enable and ollama on localhost.

## Analytics

DuckDB can't read SQLCipher, so analytics use an ephemeral snapshot:

```bash
mecha-graph decrypt --out /tmp/analytics.db   # plaintext snapshot, chmod 600
```
```sql
INSTALL sqlite; LOAD sqlite;
ATTACH '/tmp/analytics.db' AS graph (TYPE sqlite);
SELECT source, COUNT(*) FROM graph.episode GROUP BY source;
```

## Layout

- `mecha-graph-core/` — the library. Knows nothing about any agent.
- `mecha-graph-cli/` — the `mecha-graph` binary.
- `mecha-graph-mcp/` — the stdio MCP server.
- `docs/ARCHITECTURE.md` — episodes → mentions/nodes → facts → context
  packs; the trust ladder; retrieval; time and privacy.
- `docs/INTEGRATIONS.md` — per-integration auth/config and the at-rest
  design.
- `eval/` — the retrieval ruler, with the synthetic self-contained variant.

MIT licensed. Maintained alongside
[mecha](https://github.com/ljchang/mecha), whose docs site covers the
agent-side half of the story: <https://docs.mecha-factory.ai/>.
