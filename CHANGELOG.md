# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **A task's association with an entity now outlives the task.** `about`
  (task → person/project/topic) carries what a task *concerns*; `waiting_on`
  keeps its existing job of naming who holds the ball. One predicate could
  not be both, and trying made each surface wrong in a different direction:
  close the fact on completion and the finished work disappears from the
  person it was for; leave it live and their card claims they owe something
  they handed back months ago. `facts_for_node` is bidirectional, so it was
  their card carrying it, not the task's.

  Two read surfaces, because they answer different questions: `kg_task_list`
  and `mecha-graph tasks` gain `entity` (the precise query — unions `about`,
  `waiting_on` and `assigned_to`, plus tasks whose parent project is that
  node), and `kg_entity` gains a `tasks` block split into open and closed,
  capped at 15 a side with the total and a `truncated` flag. An unknown
  entity name is an error on all of them: "no tasks for her" and "there is
  nobody here by that name" are opposite findings.

  No migration — `about` and `assigned_to` were already in the seeded
  vocabulary with inverses. Nothing had ever written them for tasks.

- **`mecha-graph scan-tasks`** proposes associations by scanning task titles
  for entities the graph already knows, landing them at tier `shadow`:
  "this title contains a word that is also a name" is an inference, and
  inference is served rather than asserted. Strong matches only — a bare
  first name has no corroboration to draw on in a one-line title — and task
  nodes are excluded as targets, or every task files itself under every task
  sharing a word. Rejection memory keys on whether the pair was ever
  asserted in any state, so a refuted association is not re-minted nightly.
  Dry by default (`--apply` writes, `--limit` bounds a pass), because a
  command whose output a human is meant to judge should not have written
  everything before it prints the count.

- **`mecha-graph repair-dates`** finds date columns holding text that is not
  a date, and clears them with `--apply`. Reports by default.

### Changed (beyond the task board)

- **Accepting a candidate now honours `subject_node`/`object_node`.**
  `resolve_candidate_parts` re-resolved both endpoints from the display
  strings and never read the ids, so producers that derived a pair *from
  nodes* — `linkers` and `rules` have been setting these fields all along —
  had that thrown away at accept time, and two same-named entities collapsed
  onto whichever the lookup returned first. An explicit id now wins, falling
  back to the name when the id no longer resolves (a merge deletes the losing
  row, so the name is still reachable). This changes what accepting an
  already-queued kNN or rule candidate resolves to.

### Changed

- **Closing a task now closes its `waiting_on`**, in valid time — the
  obligation ended, it was not wrong. This changes what an existing call
  does to existing rows: before, the claim stayed live forever and every
  later read of that person carried it. The task stays findable under them,
  because the entity filter reads `fact` history rather than only what is
  live. Reopening deliberately does not resurrect the claim; who owes a
  reopened task is a new question, and guessing the old answer silently
  re-obligates someone.

### Fixed

- **`kg_upsert` now refuses a `valid_from` that is not a date.** It wrote the
  string verbatim, and at `confidence >= 0.9` auto-accepts, so prose reached
  `fact.valid_from` with no human in between — the same defect as the
  commitment path below, on the higher-volume route. Shipping `repair-dates`
  without closing this would have made the repair a treadmill: idempotent in
  its own test, dirty again by morning.

- **A model's `when` is parsed before it is stored.** `accept_commitment`
  wrote the extractor's raw string into three date columns —
  `task_detail.due_at` and the `valid_from` of both facts it asserts. A
  model answering the literal string `"null"` put that in all three, where
  it sorts as a date (lexically after any real one), so the task never read
  as overdue and it answered the wrong side of every bi-temporal `--as-of`
  query. Nothing on any surface renders `valid_from`, which is how it stayed
  invisible. Unparseable now degrades to `None` rather than failing the
  accept; the candidate payload keeps the raw value either way.

## [0.1.4] - 2026-08-31

Large for a patch, and numbered one anyway to stay in step with this
project's 0.1.x cadence: three schema migrations (V021 `fact_tier`, V022
`vec_rejected`, V023 `candidate_embedding`) and the review model's phase 2.

### Added

- **The review queue's vectors persist between runs** (V023
  `candidate_embedding`). Grouping the pending queue by similarity
  re-embedded every pending statement on every call — ~7,000 of them, ~40s
  measured — and the vectors went out with the process, so the same
  statements were embedded again for the next threshold the stepper visited,
  for the class listing after the global one, and for the TUI after the
  phone. A pending statement's text does not change while it waits, so its
  vector is immutable and re-deriving it is waste.

  A plain table rather than a `vec0` virtual one, unlike its three siblings:
  those exist to be *searched* and pay an index for it, while nothing
  searches this — the grouping fetches vectors for a known set of ids and
  clusters them in process. `text_hash` covers the model, the embed task's
  instruction and the exact text, so a model swap, an instruction change or
  an edited statement all invalidate by construction rather than by anyone
  remembering an invalidation rule. Stored as little-endian `f32` rather than
  the JSON the vec0 tables are fed: ~20MB across this queue instead of ~60MB.
  Falls through to a plain embed on any storage trouble — the cache is
  derivable and this is a read path, so a store that cannot be read must give
  a slow grouping, never a failed one.

  Measured on a copy of a live store: cross-class grouping 41.0s → 4.3s, a
  threshold step 36.9s → 4.0s, and cold and warm output byte-identical.

- **Review-on-use (phase 2).** Extraction output goes live unreviewed as a
  *shadow* fact — retrievable, rank-discounted, labeled — instead of queueing
  for review at birth, and earns a human verdict when it is about to matter.
  The loop closes: retrieval feeds the ladder and gates the extractor.
  Rejection memory survives a paraphrase (V022), so the same wrong claim
  re-extracted in different words no longer re-claims the owner's attention.

- **The entity arc.** `relink-aliases` judges the mentions already on file,
  the owner can file a merge proposal so every merge leaves a record, and
  `kg_entity` surfaces the identifiers rather than only the aliases.
  `kg_upsert kind=alias` learns the other direction.

- **`kg_notes`** — the notebook view, handing back the key that can write to
  a note.

- **Task provenance**: a task remembers what asked for it and the
  conversation that worked it, `@owner` so a harness need not know your name,
  and the board can say who holds a task rather than only that it waits.

### Fixed

- **Every group verdict re-grouped the class twice, on the UI thread.**
  `reload_review` re-groups on its own when `group_view` is set, and the
  accept and reject keys are only reachable while it is — so the explicit
  `reload_groups` preceding it ran the class's grouping a second time after
  every verdict. Both passes embedded the class before V023 and both read the
  cache after it; either way the terminal was frozen for two where one was
  needed.

- **A test fixture wore a real name**, in a repo whose export gate exists for
  exactly that. The fixtures move to the fictional cast — tracked source is
  one export away from public.

- **The queue's depth is the count before the page cut**, so a listing that
  shows less than the queue says how much less.

- **Precheck could go blind without saying so**, and commitments were exempt
  from it.

## [0.1.3] - 2026-08-22

### Fixed

- **A class's displayed accept rate counted this pipeline's own rejections as
  the owner's.** `precheck::review_clusters` summed every `status='rejected'`
  row, including the dedup and ephemeral rejects `precheck` writes itself —
  in the one view a person reads *immediately before verdicting a whole
  class*. Measured on a live store: `llm/has` displayed 18% against a true
  67% over 48 human verdicts, `llm/has_role` 7% against 53%, `llm/attended`
  39% against 81%, and three classes displayed a 0% accept rate on which no
  human had ever voted at all. `ladder::human_record` had carried the correct
  filter (`reject_reason NOT LIKE 'precheck:%'`) since it was written; the
  cluster view never did. Machine rejects are now reported beside the rate as
  `machine_rejected` and never inside it — a class that mostly repeats itself
  is a different problem from one that is mostly wrong.

- **`review --json` no longer prints prose ahead of the array.** An empty
  result emitted `no pending candidates` and then `[]`, so the whole of stdout
  failed to parse and a caller asking for an empty set got a JSON error
  instead of `[]`.

### Added

- **`review --proposers` — the queue rolled up by proposing mechanism**, with
  each one's *human* accept rate and Wilson lower bound, and `p` in the TUI
  for the same view. A proposer spreads across many predicates (the extractor
  alone holds ~90), so its own hit rate is invisible in a list of 733
  `(proposer, predicate)` rows — and mechanisms are what get switched on,
  tuned, and switched off. An unjudged mechanism shows a dash, never 0%:
  "never reviewed" and "always rejected" are opposite findings.

- **`review --sample N [--seed S]` — a uniform random draw** from what
  `--proposer` / `--predicate` left. The queue is ordered, every order it
  could have is correlated with something, and judging the first N then
  reading the result as a class's accept rate measures the ordering. The seed
  is printed when not supplied, because a sample nobody can redraw is a sample
  nobody can check. Partial Fisher–Yates over a four-line splitmix64, with a
  uniformity test over 4,000 draws that fails on `truncate(k)`.

- **`review --proposer` / `--predicate`** filter the item view, matching on
  `precheck::cluster_key` so a drill-down can never show a different set than
  the cluster row it came from.


## [0.1.2] - 2026-08-22

### Fixed

- **The binary stopped introducing itself as `pkg`.** `#[command(name = "pkg")]`
  survived the 0.1.0 rename, so `--version` printed `pkg 0.1.1` and `--help`
  read `Usage: pkg` — on a crate whose front door is `cargo install
  mecha-graph`. Three of the five sites were worse than cosmetic because they
  told the reader to *run* something: a usage hint after a DB move, the review
  nudge in `render.rs` (`→ pkg review`), and `mecha-graph-mcp`'s open-failure
  prefix. All five renamed; `PKG_*` env vars and the `~/pkg` data dir are
  untouched, being a migration rather than a rename.

## [0.1.1] - 2026-08-21

### Changed

- **One engine, one model, chosen by measurement.** ollama is gone; embeddings
  are served by llama-server on `:8081`, beside the chat model, and the
  embedder is **harrier-oss-v1-0.6b**. Four candidates were scored on 80
  semantic queries against a 5,000-episode pool: harrier took MRR 0.3595 and
  recall@10 0.562 against the nomic incumbent's 0.2708 / 0.4375. Re-embedding
  the live store takes about nine minutes and 175 MB.

  **MTEB did not predict this.** Qwen3-Embedding-0.6B scores 70.70 on MTEB
  English v2 against nomic's ~62 and *tied* it here; harrier beat Qwen3 by 26%
  at identical size, which also kills the bias flagged before the run — the
  queries were generated by a Qwen model, and the Qwen embedder still lost.
  Qwen3-4B's 0.028 MRR edge over harrier is inside the standard error at n=80
  with identical recall@10, so it is not separable and not worth 6.7× the size.

- **`SEMANTIC_DUP_THRESHOLD` 0.93 → 0.97**, flag threshold unchanged at 0.83.
  A similarity threshold encodes *one model's cosine scale* and nothing about
  these said so: nomic's range is compressed, putting unrelated text at 0.56
  where harrier puts it at 0.28, so carrying 0.93 across would have silently
  stopped matching anything. Recalibrated by matching the operating point —
  sweeping `dedupe-facts` over the same corpus against a pre-migration backup
  and the re-embedded store — which preserves the *rate*, not the accuracy.
  `precheck.rs` carries the calibration table at the constants it explains.

- **The README meets the stranger it now has.** Install from crates.io as
  the front door, the synthetic eval as the try-it-with-no-data path,
  wiring blocks for mecha (unprefixed, marked untrusted — the interlock
  reasoning stated), Claude Code, and any MCP client, and the privacy
  story gathered into one section: encrypted at rest, stream-first
  ingestion, the sensitivity ladder, true delete, local by construction.
- **The self-improvement plan is a design document.** `docs/PLAN.md` now
  states the three doctrine decisions (autonomy by exception, graph-first
  with session-splitting, the mechanical error contract), the five gossip
  roles, the mechanism catalog with build waves, and every settled design
  point with its rationale — in an impersonal voice.

### Added

- **`embed_meta` (migration 16)** records which model produced the live
  vectors. Nothing about a vector reveals what made it, and a 768-dim nomic
  vector is indistinguishable from a truncated 768-dim Qwen one.
- **`docs/EMBEDDING-RESEARCH.md`** — the candidates, the two measurement
  attempts that measured nothing and why, the results, and the failure
  analysis. Read it before changing an embedder or a threshold.

### Fixed

- **`--pooling last` for decoder-only embedders.** `mean` does not error; it
  silently produces plausible, worse vectors. The serving flags and the numbers
  behind them live in `~/.local/bin/mecha-embed-server`.


## [0.1.0] - 2026-08-16

The first public release: a clean-room extraction of a private research
repository. History starts here on purpose — the development record was a
journal of the data the project exists to hold, and no filter makes a
journal safe.

### Added

- **Three crates**: `mecha-graph-core` (ingest, enrich, resolve and link,
  retrieve — knows nothing about any agent), `mecha-graph` (the CLI), and
  `mecha-graph-mcp` (a stdio MCP server any client can sit on), published
  to crates.io.
- **Eleven MCP tools**: `kg_search`, `kg_entity`, `kg_timeline`,
  `kg_related`, `kg_upsert`, the verification family (`kg_verify`,
  `kg_pending`, `kg_verdict`), and a small task family. The tools carry
  their own `kg_` namespace, so harnesses that support unprefixed
  registration can drop the server prefix.
- **A synthetic eval world**: `eval/synthetic/run.sh` builds a throwaway
  graph from a fictional corpus (a twelve-message mailbox, a small
  calendar) and grades 24 retrieval queries — no personal data, no live
  store, works on a fresh clone.
- **Encrypted-at-rest store** at `~/.mecha-graph/graph.db` (SQLCipher; the
  raw key beside it, mode 0600), `MECHA_GRAPH_*` environment overrides,
  stream-first ingestion, `capture_delete` retention for file sources, a
  four-level sensitivity ladder with private-by-default retrieval
  exclusion, and true delete (`redact` purges an episode and everything
  derived from it).

[Unreleased]: https://github.com/ljchang/mecha-graph/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/ljchang/mecha-graph/releases/tag/v0.1.0
