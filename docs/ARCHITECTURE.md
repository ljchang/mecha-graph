# Architecture — the mental model

One sentence: **episodes are evidence, nodes are things, facts are beliefs,
and the context pack is the product.** Everything in pkg is machinery that
turns raw records of your life into a token-bounded, provenance-carrying
slice an agent can trust.

```
 SOURCES          EVIDENCE            WIRING              BELIEFS           PRODUCT
 (bee, cal,   ┌─────────────┐   ┌──────────────┐   ┌─────────────────┐   ┌─────────┐
  slack,   ──▶│  episodes   │──▶│   mentions   │──▶│ fact candidates │──▶│ context │
  github,     │ (append-    │   │ episode↔node │   │  → review →     │   │  pack   │
  sessions,   │  only raw   │   │   (M:N)      │   │  facts (bi-     │   │ (ranked,│
  reflect,    │  records)   │   │              │   │  temporal)      │   │ bounded)│
  mbox, …)    └─────────────┘   └──────┬───────┘   └─────────────────┘   └─────────┘
                                       │
                                ┌──────┴───────┐
                                │    nodes     │  people · projects · orgs ·
                                │  (entities)  │  topics · places · tasks · …
                                └──────────────┘
```

## The four layers

### 1. Episodes — evidence (append-only)

An episode is the raw record that *something happened*, before any
interpretation: one Bee conversation, one calendar event, one Slack
channel-day, one GitHub repo-day, one agent session, one Reflect note.

- Keyed by `(source, source_id)` — re-ingest is **idempotent**; a
  `content_hash` detects edits (e.g. a re-exported Reflect note updates in
  place, never duplicates).
- `occurred_at` is when it happened in the world (not when we saw it —
  that's `ingested_at`; the distinction matters below).
- `sensitivity` tiers gate retrieval: `private` episodes (all Bee
  transcripts) are excluded from default search; callers opt in.
- Episodes are **never edited or deleted**. The two exceptions are
  deliberate: `mecha-graph redact` (true delete, §10) and nothing else. When a fact
  turns out wrong you supersede the fact — the episode that spawned it
  stays, because it's the answer to "where did we learn this?"
- For file-based sources the full original is archived to `episode_raw`
  *inside the encrypted DB*, verified, and only then is the plaintext file
  deleted (`capture_delete`). Stream sources never touch disk at all.

### 2. Nodes — things (the entity layer)

People, projects, orgs, topics, places, goals/areas/tasks, events,
documents. A **closed type set** — extractors return null rather than
invent types, because open-ended types cause junk-node explosion.

Identity is layered, strongest first:

- `node_identifier` — deterministic keys (email, phone, slack_uid, url,
  cwd/path). Two sources asserting the same identifier ARE the same
  entity; this merges with no model and no review.
- `node_alias` — names and nicknames, indexed. Aliases that map to more
  than one node ("two Victors") **never auto-link**; ambiguity is
  surfaced to the caller, and a user's answer is written back as a
  permanent alias — resolution *learns*.
- `canonical_name` — the display name, scanned alongside aliases.

Event/document nodes are retrieval *targets*, never query anchors: a
meeting literally titled "Nadia" must not shadow the person.

### 3. Mentions — the wiring (episode ↔ node, M:N)

`mention` rows connect episodes to the nodes they involve. This table is
load-bearing far beyond what it looks like:

- **Entity timelines** ("show me everything with June") are mention scans.
- **Filter-first retrieval** (§8.1): when a query names an entity, the
  candidate set collapses to episodes *mentioning* that node before any
  ranking runs. An episode whose text says "flowmail" but has no mention
  edge is invisible to a flowmail-anchored query — text match is not
  membership. (This is why linking quality matters more than ranking.)
- **Co-occurrence statistics** (NPMI) that propose edges read this table.

Mentions carry their `extractor` (attendee, alias, backlink, temporal_join,
reflect, manual) and a confidence — provenance all the way down.

### 4. Facts — beliefs (interpreted claims, bi-temporal)

A fact is a claim connecting nodes: `Nadia –works_at→ Bayview Institute`, with
a natural-language `statement` ("Nadia works at Bayview Institute.") because
sentences embed and retrieve well while triples traverse well — store
both, embed the sentence, walk the triple.

- **Two timelines per fact**: *valid time* (when it was true in the world:
  `valid_from`/`valid_to`) and *system time* (when we believed it:
  `ingested_at`/`invalidated_at`). You can ask "what was true in March"
  and "what did I believe in March" separately. Supersede, never delete.
- **Predicates are a controlled vocabulary** (with an alias table), or
  `works_on`/`working_on`/`is_working_on` become three relations.
- `observation_count` accumulates corroboration: re-asserting an existing
  live fact bumps the count instead of duplicating. Re-observation is
  evidence.
- Every fact points at the episode it came from.
- **Beliefs have a polarity.** A negative fact ("Nadia does *not* work
  at NYU") is rejection memory: it records that something was asked and
  answered, so nothing re-proposes it. Traversal (`fact_current`, the
  `edges` view, linkers, GTD, stats) is positive-only — a negative edge
  would be a bug — while every *display* path serves both polarities,
  because the surfaces where you'd re-ask are exactly where a denial has
  to be readable.

**Three ways a belief stops being current**, and the difference is not
cosmetic — it is what the two timelines are *for*:

| | Means | Sets |
|---|---|---|
| supersede | replaced by a better value | both timestamps |
| decay | true then, false now (the world moved) | valid time only |
| never-true | we were wrong to believe it at all | system time + a zero-length valid window |

Only decay leaves `invalidated_at` NULL, so `facts_as_of` keeps
answering correctly for the period the belief actually held, and no
producing class is blamed for having been right at the time. A
never-true retraction collapses valid time to a point so no as-of date
serves it. All three remove a fact from retrieval; `kg_timeline` still
shows the whole history.

## How episodes become facts: the trust ladder

Nothing skips the ladder. Each rung is cheaper-and-more-precise first
(§7), and the rung determines whether the result writes directly or must
be staged for review:

| Rung | Mechanism | Writes |
|---|---|---|
| Deterministic keys | email / slack_uid / cwd / attendee lists | direct (mentions, identity) |
| User-authored | TUI capture, Reflect typed notes, `b`-bind aliases | direct, high confidence |
| Alias/name scan | known names found in episode text | direct mentions (unambiguous only) |
| Temporal join | Bee recording overlaps calendar meeting | direct mentions, confidence < 1 |
| Statistical (NPMI) | frequency-corrected co-occurrence | direct `related_to`, capped confidence |
| Embedding kNN | mean-centered node centroids, similar contexts | **staged** as candidates |
| Structural (Adamic-Adar) | shared graph neighborhoods | **staged** as candidates |
| LLM extraction | gemma reads episodes, proposes facts | **staged** as candidates |

The staging queue (`fact_candidate`) is the membrane between "a model
said so" and "the graph believes it": **extraction proposes, promotion
disposes.** `pkg precheck` auto-triages that queue — duplicates of known
facts are rejected with an observation bump, in-queue repeats collapse,
conversational recaps ("X discussed Y") are dropped as bloat since the
episode already records them, contradictions on single-valued predicates
are flagged and always held for a human, and (opt-in) clean novel facts
on durable predicates auto-accept. What reaches the TUI review screen is
meant to be only what genuinely needs a decision.

## Retrieval — filter first, rank second

Embedding "when did I last meet June?" yields a vector about the
*question*, not about June. So the router (§8):

1. **Detects entities** in the query deterministically (alias +
   identifier scan — sub-millisecond, not an LLM), plus `#tag` filters
   and time expressions.
2. **Classifies intent**: LOOKUP ("when did I last…") is answered from
   the `person_interaction` rollup with no embeddings at all; AGGREGATE
   ("who do I interact with most") reads rollups/counts; RECALL does
   hybrid search.
3. **Filters first**: entity and tag filters collapse the candidate set
   via mentions *before* ranking.
4. **Ranks** with BM25 (porter-stemmed FTS5) + vector similarity fused by
   RRF, facts and episodes competing on one scale.
5. **Packs**: the result is a context pack — ranked items with kind,
   source, timestamps, and provenance ids, truncated to a token budget.
   If entity resolution was ambiguous, the pack says so and the consumer
   is expected to ask, not guess.

Only *live* beliefs are served: the search indexes cover every row ever
written, so retrieval filters retracted facts explicitly rather than
trusting the index to forget them.

The envelope carries two further self-descriptions, both omitted when
they have nothing to say:

- **`flags`** (≤2) — problems pkg detected in what it is about to
  return: a contradiction on a single-valued predicate, a denial
  contesting a served belief, a fact past its predicate's half-life.
  pkg detects with provenance; the consumer judges whether to act. Same
  division as ambiguity, generalized.
- **`scope`** — whether this pack could see facts, evidence, or both.
  A verifier has to know what an answer *could* have drawn on;
  `facts_only`/`evidence_only` are how two readers can be given
  deliberately blind halves of the same question.

## Time, privacy, and ops

- **Future episodes are not interactions**: a scheduled meeting is not
  "last met". Rollups exclude `occurred_at > now`.
- **At rest**: the DB is SQLCipher-encrypted (`~/pkg/db.key`, 0600);
  plaintext source files are deleted after verified capture; analytical
  snapshots come from `pkg decrypt` and are transaction-pinned.
- **Nightly** (03:30 cron): source sync → bee-facts two-way sync →
  linker cascade → GPU-gated embed + LLM extract → precheck → scope
  summaries → `MEMORY.md` boot context → health alerts. Everything is
  cursored and idempotent; a missed night just catches up.
- **Eval**: `eval/gold.jsonl` is a regression guard run after any
  router/linker change. Recall@10 = 1.00 is the floor, not a score.

## Boundaries — what lives here, what lives in the agent

pkg and mecha (`~/Github/mecha`) are deliberately separate repositories
with **no compile-time dependency in either direction**. The entire
interface is the MCP tool namespace (`pkg__kg_search`, `pkg__kg_upsert`,
…); mecha's own eval suite tests against a *fixture* pkg server, not
against pkg itself. Settled 2026-08-12; the reasoning is worth keeping
because it will be re-litigated.

### Why separate

**Durability asymmetry — the decisive argument.** pkg holds an
encrypted, migration-versioned store of a life; it is irreplaceable if
corrupted. mecha holds agent behaviour, which is replaceable and
*should* be replaced as models and harnesses change. You do not fold a
durable asset into a disposable tool. Expect pkg to outlive whatever
harness is currently in front of it.

**The boundary carries the security model.** mecha's taint interlock
treats a pkg read as arming both taint legs, and mecha's eval has cases
asserting exactly that (`web-then-memory`: taint private+untrusted,
`blocked_sends: 0`). That analysis only works because reading pkg is an
*external* act. Merge the two and "reading my own memory" versus
"reading pkg" blurs precisely where it currently needs to be sharp.

**§2 is only enforceable across a crate boundary.** "pkg-core knows
nothing about any agent" has repeatedly produced better designs by
pushing orchestration out — the `ask_ada` route, pack flags that
*describe* rather than decide, pkg not writing into mecha's mailbox. In
one workspace that constraint erodes by convenience.

**Multiple consumers.** Claude Code and Hermes over MCP, the `pkg` CLI,
DuckDB for analytics; FlowMail is a future consumer on macOS. Even at
one real consumer the MCP surface costs nothing already being paid.

### The two invariants that keep the split clean

Both are current practice. Violating either is what would actually
create redundancy between the repos:

1. **pkg's own interface stays non-conversational** — commands,
   tables, keystrokes. The moment pkg grows a chat surface there are
   two harnesses.
2. **mecha never stores facts** — it produces episodes through
   `kg_upsert` and reads context packs. The moment mecha caches graph
   state there are two graphs and a sync problem.

### Two interfaces because there are two modes

The direct interface (`pkg` CLI + TUI) is load-bearing, not a
convenience:

- it is the **unmediated correction channel** — if the only way to fix
  the graph is to ask an agent, there is no ground-truth path;
- it is the **audit surface for autonomy** — auto-accepting classes of
  fact is only safe if their output can be inspected without a model
  in between;
- **bulk work is keyboard work** — cluster review with a marked set
  beats conversing about a thousand candidates.

| Mode | Surface | For |
|---|---|---|
| conversational, in-context, reactive | mecha | point-of-use flags, questions, corrections in flow |
| direct, bulk, deliberate | `pkg` CLI/TUI | cluster review, schema authoring, health, forks |

Most apparent duplication between the repos is nominal — the same word
for different jobs. pkg's review queue holds *world facts*, mecha's
holds *behaviour rules*. pkg's `sensitivity` is static classification
on a row; mecha's `taint` is dynamic flow control on a conversation.
pkg's eval measures retrieval quality; mecha's grades agent traces.
Only two overlaps are real: the class/outcome ledger (same state
machine, different substrate — share the written mechanism, implement
twice) and scheduling (mecha's `cron.rs`/`trigger.rs` is the better
one; `scripts/nightly.sh` stays as the standalone fallback).

### pkg is the agent's declarative memory

ACT-R — already borrowed for base-level activation (§11.5) — splits
**declarative** memory (chunks) from **procedural** memory (production
rules). That split answers "should pkg be mecha's memory system?":

| Memory | Content | Home |
|---|---|---|
| **semantic** (declarative) | facts about people, projects, orgs | **pkg** |
| **episodic** (declarative) | what happened, including the agent's own sessions | **pkg** (`sources/sessions.rs`) |
| **procedural** | reflexion rules — "check the graph before searching the web" | mecha `~/.mecha/learning/` |
| **working** | the live conversation, compaction | mecha runtime (state, not memory) |
| **operational** | outbox, triggers, messages, liveness | mecha files (infrastructure) |

So pkg already *is* mecha's declarative memory: the session-end
distiller writes episodes through `kg_upsert`, `sources/sessions.rs`
ingests sessions, and §8.3's boot digest supplies opening context.

The litmus for anything new is the one that already routes corrections:
**"would the user ask an assistant about this later?" → pkg. "Should
the agent behave differently next time?" → the harness.**

Procedural memory stays out of pkg even though bi-temporality,
provenance and supersede would all be useful for rules — it is not
world knowledge, it is model-specific, and a harness swap should not
inherit the previous harness's habits. Revisit only if rules reach the
hundreds.

The boundary that genuinely blurs: *agent* episodic memory with
retrieval ("what did I try last time this build failed?"). Session
*content* is often world knowledge and belongs here; the operational
residue ("tried X, it failed") is procedural and does not.

### One graph, not many

`PKG_DB`/`--db` makes a second graph free today, and it is still
usually the wrong move: a second graph **splits the entity space**.
The same person in two graphs means schema drift, identity mismatch,
no cross-graph query, and a reconciliation problem needing machinery
this project deliberately declined to build.

Partition inside one graph instead — `sensitivity` tiers, annotation
tags, and the `scope_id` parent chain (§4.5) are the axes that already
exist. Note `scope_id` is hierarchical containment, not tenancy.

The one case where a separate graph is right is a **shared or team
graph** with genuinely different ownership and access. That is real
federation, and it is the condition under which the parked
peer-to-peer reconciliation work (Semantic Gossiping, cycle
consistency — covered in the internal research notes) becomes relevant again.

## A worked example

A Reflect note titled `Iris Calder` with `Type: #person`,
`Email: iris.calder@example.com`, `Company: Westfield`:

1. `pkg ingest reflect` streams it from the export zip → **episode**
   (`reflect.note`, keyed by the note's stable id), raw markdown archived,
   zip deleted after verification.
2. `pkg reflect-process` sees the type tag → resolves the email
   **identifier** → attaches to the *existing* Iris node rather than
   creating a duplicate; `Company: Westfield` becomes a **fact**
   (`works_at`, extractor `reflect`, pointing at this episode); the
   episode gets a **mention** of Iris.
3. `pkg link --auto` scans every episode for known names → more mentions;
   NPMI notices who co-occurs with Iris unusually often → `related_to`
   facts; kNN and Adamic-Adar stage speculative candidates for review.
4. A query for "iris westfield" detects the entity, collapses to episodes
   mentioning her, ranks, and returns a pack whose items each carry the
   uid you'd need to trace any claim back to this note.
