# The CLI

One binary, `mecha-graph`, with subcommands grouped here by the job you are
doing. Everything honours three global flags:

```
--db <DB>    Database path (default: ~/.mecha-graph/graph.db, or $MECHA_GRAPH_DB)
--json       Machine-readable output (the default when piped)
--text       Human-readable output (the default on a terminal)
```

That last pair means every command is scriptable as-is: pipe it and you get
JSON, no flag needed.

## Set up and feed

| Command | What it does |
|---|---|
| `init` | Initialize the database (runs migrations; safe to re-run). |
| `source add <kind>` | Register an integration — `ics --url … --me you@…`, `mbox --path … --retention capture_delete`, `slack --token …`, and the self-registering kinds. Config lands in `~/.mecha-graph/config.toml`. |
| `source list` | Every source: kind, enabled, auth state, last ok, item count. |
| `source sync [name]` | Ingest everything enabled (or one source). Cursored and idempotent — re-runs are no-ops. |
| `source remove <name>` | Unregister; already-ingested episodes are kept (`redact` purges). |
| `ingest` | One-off ingestion of a single source, for scripting around `sync`. |
| `link` | Re-run the deterministic linkers and rollups over existing episodes — alias scan, temporal join, NPMI — after new aliases land, entities pick up their mentions. The candidate-staging tiers (kNN, structural, rules) run only with `--propose`: they measured 4–14% human accept with nothing consuming the rate, so proposing is opt-in until a precision gate exists. |
| `embed` | Embed pending episodes and facts via the llama-server embedding endpoint (:8081). Rebuilds the `vec0` tables if `[llm] embed_dims` changed, which discards every stored vector — see docs/INTEGRATIONS.md. Batch it when the GPU is free; nothing else waits on it. |
| `extract` | LLM extraction over pending episodes → fact *candidates* for review. The expensive tier, deliberately separate from ingestion. |

## Ask

| Command | What it does |
|---|---|
| `query "<question>"` | The main event: returns a context pack (JSON) — token-bounded, provenance-carrying, freshness-stamped. `#tag` tokens filter to episodes carrying all those tags; a tag alone lists them newest-first. |
| `entity "<name>"` | Everything about an entity: identifiers, aliases, facts with provenance, timeline. |
| `facts` | Browse facts; `--tag` revisits what you marked in the TUI. |
| `episodes` | Browse episodes. |
| `raw <uid>` | The archived raw content behind an episode, from inside the encrypted store. |
| `stats` | Health stats: episodes by source, nodes by type, live facts, enrichment/embedding coverage. |
| `summarize` | Refresh generated entity-scope summaries. |
| `memory-md` | Generate the boot-injection memory file (~500 tokens) — a digest an agent can load at session start. |
| `tags` | Every tag in use. |

## Curate — the review loop

Since review-on-use (docs/REVIEW-ON-USE.md), extraction output no longer
queues for review at birth: clean candidates go live as **shadow facts** —
retrievable, rank-discounted, labeled `unreviewed` — and earn a human
verdict when they are about to matter. What still queues is what cannot
exist as a fact without a human: commitments, precheck-flagged
contradictions and near-duplicates, and unresolvable subjects.

| Command | What it does |
|---|---|
| `shadow` | The surfaced-verdict queue: live shadow facts that are about to matter — contradicting a reviewed fact, actually served in a context pack, or spot-checked by a sampled class. `--confirm <uid>` promotes to reviewed; `--refute <uid> [--reason …]` retracts as never true (the reason feeds rejection memory). At most ten at a time: the human is the scarce resource. |
| `shadow-convert` | One-shot: bulk-convert the standing pending backlog to shadow facts under the same held-classes rule the ingest path applies. |
| `calibrate-groups` | Measure the cascade thresholds against the recorded human verdicts: at each cosine floor, how often two decided statements that close carried the same verdict — split same-class vs cross-class, Document vs Dedup space. The 2026-08-29 run: same-class ~89% flat across floors, cross-class ~63% at every usable floor — which is why cross-class cascades warn and the TUI group view never crosses. |
| `utility` | The utility loop's report: per-class retrieval record (facts old enough to have had a chance, and whether any query ever pulled them), what the precision gate blocks from extraction, and — with `--floor`, `--apply` — utility ladder demotions. One grep-able summary line for the nightly log. |
| `review` | The pending fact-candidate queue. `--clusters` groups it by (proposer, predicate); `--proposers` rolls it up by proposing mechanism with each one's **human** accept rate — machine rejects are reported beside the rate, never inside it, and a mechanism nobody has judged shows a dash, not 0%. `--proposer` / `--predicate` filter, and `--sample N [--seed S]` draws uniformly at random from what the filters left: the queue is ordered, every order is correlated with something, and judging the first N measures the ordering. The seed is printed so a sample can be redrawn and checked. |
| `accept` / `reject` | Decide candidates by id, or in bulk by filter (`reject` records the reason). |
| `precheck` | Auto-triage the queue: drop duplicates (against the graph and within the queue, exact and paraphrase — the embedded rejection memory catches a rejected claim rewritten), flag contradictions, auto-accept what the ladder earned, and mint the clean rest as shadow facts. Run it before reviewing by hand. |
| `corrections` | The corrections ledger — what arrived from agents saying the graph was wrong, and what was done about it. |
| `note` | Quick note capture; entities are auto-linked. |
| `annotate` | Tag or note an existing episode. |
| `merge <keep> <dup>` | Merge two entities; `dups` lists same-name candidates first. |
| `fix-person-names` | Promote a human alias to the display name where a person node is named by an email address — the address keeps resolving; only what renders changes. |
| `dedupe-facts` | Collapse duplicate facts. |
| `owner <name\|email>` | Declare who "I" is, so self-references resolve. |
| `reflect-process` | Promote structured notes (`Type: #person/#company/#book…`) to entities with identifiers and facts. |
| `bee-facts` | Two-way wearable-facts sync: pull unconfirmed suggestions into the review queue; push your verdicts back. |

## Tasks

| Command | What it does |
|---|---|
| `gtd` | The task board: next / inbox / waiting / scheduled. |
| `tasks` | List and update tasks from the shell. |

## Maintain and repair

| Command | What it does |
|---|---|
| `decay` | Re-derive every co-occurrence belief; close the ones whose statistic collapsed (valid time only — decay is not error), refresh drifted numbers, alarm on input-set collapse. Nightly. |
| `verify` | The deterministic verifier tier: dereference a claim's provenance and report what the rows actually say. A lexical miss is residue for a model to judge — never a refutation by itself. |
| `probe-targets` | Rank entities by demand × slot-gaps × staleness (SQL only). Feeds the gossip harness. |
| `recompute-confidence` | Re-derive stored confidence from the observation history. |
| `invalidate-phantoms` | One-shot repair: retract co-occurrence beliefs with zero remaining support. |
| `backfill-derivation` | Retrofit provenance onto derived facts written before derived-fact provenance existed. |
| `tombstone` | Deletion tombstones — what re-ingest is blocked from resurrecting; `tombstone rm` lifts one. |
| `undo` | Undo the most recent TUI episode delete/edit (Ctrl-Z inside the TUI). |
| `eval` | Run the gold-set eval against your graph (`eval/synthetic/run.sh` in a checkout is the no-data variant). |

## Data safety

| Command | What it does |
|---|---|
| `encrypt` / `decrypt` | Move between encrypted and plaintext copies. `decrypt --out /tmp/analytics.db` makes the ephemeral snapshot DuckDB can attach. |
| `fork` | A full encrypted copy under a fresh key — the test bed for experiments that must not touch the live store. |
| `redact --episode <uid>` | True delete: the episode, its raw archive, mentions, embeddings, FTS rows, enrichment, and derived facts. |

For the interactive counterpart to the review loop, see [the TUI](TUI.md).
