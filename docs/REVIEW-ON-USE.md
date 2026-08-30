# Review-on-use — phase 2 of the review-queue overhaul

Design captured 2026-08-28 from the flood-diagnosis session (see
HANDOFF.md session 15 for what phase 1 shipped and the numbers behind
it). **Status: BUILT 2026-08-29**, all five components — §4 closed by
measurement the same day (see "§4 — resolved" below). This doc is the authority
for the phase-2 arc; where it conflicts with RESEARCH_LOOP.md, the
decision layer is still PLAN.md.

## The problem phase 1 did not solve

Phase 1 cut generation (calendar out, caps, speculative tiers off, bee
retired) and hardened dedup, but the review model is still
**review-on-write**: every surviving candidate claims a slice of the
owner's attention at staging time, ordered by confidence — a number the
extractor made up. The two measured facts that indict this model:

- Only ~17% of live facts were ever served in a context pack
  (owner-asserted 43%, llm 21%, calendar-attendee 10%). Accept rate
  measures "not wrong"; retrieval measures "worth having". They
  disagree, and review currently optimizes the wrong one.
- The queue's marginal item is trivia that is *true*. The owner's 68%
  accept rate on llm facts co-exists with the complaint that review is
  a waste of time — because plausibility-ordered review fills with
  true-but-inert claims.

The inversion: **the human is the scarcest resource; retrieval is the
only ground truth of usefulness. A fact earns review when it is about
to matter, not when it is born.**

## Components

### 1. The shadow tier

Extraction output lands as *shadow facts*: retrievable, but marked.

- Storage: live rows with a `tier` column (`shadow` | `reviewed`) on
  `fact` — NOT a parallel table; every consumer (`hybrid_facts`,
  `facts_for_node`, context assembly) sees one schema. A closed enum
  written to an append-only store is a wire format: unknown tier loads
  as `shadow` (fail-closed: unreviewed).
- Context packs may include shadow facts but must label provenance
  (`unreviewed` beside the existing extractor/confidence), and ranking
  discounts the tier — a shadow fact never outranks a reviewed fact at
  equal relevance.
- Accepting a candidate today = minting a `reviewed` fact. The
  fact_candidate queue as a *human surface* disappears; precheck's
  machine tiers keep running against shadow facts at ingest (dedup,
  contradiction, ephemeral) since they cost no attention.

### 2. Demand-driven review triggers

A shadow fact is surfaced for verdict only when:

- it was served in a context pack (`retrieval_touch` fires) — review
  arrives a handful at a time, in context, about facts doing work;
- its subject entity is opened in the TUI/entity view;
- it contradicts a reviewed fact (the existing contradiction tier);
- or a class-level spot-check samples it (the ladder's `sampled` rung —
  the uniform `--sample` draw already exists for exactly this reason).

The surfaced set is small by construction (~10 facts/query max). The
verdict UI shows the fact *beside the query that pulled it* — the
reviewable object is the thing itself.

### 3. Close the utility loop

The signal exists (`stats` → `fact_usage`, shipped in phase 1); nothing
acts on it yet.

- **Ladder demotion**: a class whose retrieval rate sits under a floor
  after N serves-worth of opportunity demotes a rung, beside the
  existing human-verdict promotion. Demotion on statistics was
  explicitly refused for *human accept rate* (ladder.rs header — a
  reject is not a demotion); utility is a different signal with a
  different owner (the query stream, not the reviewer) and does not
  carry that objection. Ratify with Luke before building.
- **Generation gating**: a (proposer, predicate) class below a
  precision floor (`accept_lb`, computed since 08-16, consumed by
  nothing) or a utility floor stops being *extracted at all* — the
  predicate drops out of the extraction prompt's enum for that source,
  or the proposer skips the class. This is the piece that makes the
  system self-limiting instead of self-flooding.
- A nightly one-line report: classes gated, classes demoted, and why —
  a guard that acts silently is the failure mode this repo keeps
  finding.

### 4. Cluster review is the only bulk surface

For whatever backlog remains human-facing (the 7,049 llm facts today):
global embedding clustering (the `similar.rs` machinery), cluster-level
verdicts via cascade, surfaced **in the TUI** (today it is CLI-only and
undocumented). Calibrate the global threshold against the ~2,400
recorded human verdicts instead of the guessed 0.90; consider
`EmbedTask::Dedup` space (implemented, wired to nothing) with
recalibration — cosine scales are not comparable across task prefixes
any more than across models.

### 5. Semantic rejection memory

A human reject writes a durable suppression record: the statement
embedding (Dedup space) joins a rejected-set index that precheck's
semantic tier checks alongside live facts — today rejection memory is
exact-normalized-string only, and the mid-August model swap guarantees
paraphrase leaks. Pair-level suppression for speculative proposers
already shipped in phase 1.

## Open decisions — RESOLVED 2026-08-29 (asked, answered by Luke)

1. Utility-based ladder demotion — **ratified**. The distinction from the
   refused accept-rate demotion is recorded in ladder.rs's header: utility
   is a different signal with a different owner (the query stream, not
   the reviewer); a human reject still never demotes. Demotion is ONE
   rung per run (`ladder::utility_demotions`), never straight to staged.
2. Shadow facts in context packs — **on by default, labeled**. The tier
   discount is `search::SHADOW_DISCOUNT` post-fusion; the label rides
   `PackItem.tier`, the rendered `[fact · unreviewed]`, the context
   line's `[UNREVIEWED] ` prefix, and `kg_entity`'s `tier` field.
3. Floor and window — **still deferred on purpose**: `UtilityFloors` are
   parameters, not constants; the nightly runs `pkg utility` report-only
   until `UTILITY_FLOOR` is set in nightly.env. The precision gate
   (`accept_lb < 0.15` over ≥ 20 human verdicts) IS live — the human
   record has tenure the usage data does not.
4. Backlog — **bulk-convert** (`pkg shadow-convert`). The backlog
   disappears as a concept; held classes (commitments, flags,
   unresolvable subjects) stay queued under the same rule as ingest.

## As built (2026-08-29) — deltas from the sections above

- **Tier is a wire format**: `fact.tier` TEXT, and every consumer tests
  `tier <> 'reviewed'` — an unknown tier reads as shadow, never as
  reviewed. Corroboration takes MAX tier like it takes MAX sensitivity.
- **The candidate row survives as bookkeeping**: a mint sets
  `status='shadow'` + `fact_uid`, with reviewer columns NULL. A verdict
  on the served fact settles it (`confirm_shadow_fact` /
  `refute_shadow_fact`) — only then does it enter `HUMAN_VERDICT_SQL`
  and move the ladder, once per keystroke (the cascade rule).
- **Precheck's contradiction and similar flags still HOLD** rather than
  mint: minting a flagged near-twin puts both twins in retrieval, and a
  contradiction resolution may need to supersede the *reviewed* side,
  which is a correction, not a mint. §2's contradiction trigger is
  served by `shadow::surfaced` over facts instead.
- **Verdict verbs are not on the MCP surface.** `kg_shadow_queue` is
  read-only; confirm/refute live in the CLI and TUI only — an agent
  relaying the owner's yes is a paraphrase, and a lane must not promote
  itself. Revisit only with a real identity story on the MCP channel.
- **Rejection memory** (§5) went in as `vec_rejected` (V022), Document
  space at precheck's existing 0.97 — NOT `EmbedTask::Dedup`, which
  stays wired to nothing until someone calibrates its scale against the
  recorded verdicts (cosine scales don't transfer across task prefixes).
- **Extraction gating is structural**: the gated predicate leaves the
  grammar enum (`extraction_predicates`), so the waste never happens;
  the run prints what it gated, `pkg utility` prints one grep-able line,
  applied demotions land in `event_log`.

## §4 — resolved 2026-08-29 (calibrated, and the measurement decided)

`pkg calibrate-groups` measured cascade agreement against 2,423 recorded
human verdicts, split by class relationship. The finding replaced the
question:

- **Same-class pairs: ~89–90% agreement, flat from cosine 0.80 to 0.96.**
  The (proposer, predicate) class carries the kinship signal; the cosine
  floor barely adds to it. The within-class cascade is sound at the
  existing 0.83.
- **Cross-class pairs: 59–67% at every floor below 0.97** (and ≥0.97 is
  precheck's dedup line, where pairs stop existing). A cross-class
  cascade at ANY usable floor overwrites the owner's own counterfactual
  verdict on ~1 pair in 3. The guessed 0.90 was not miscalibrated — the
  layer is unsound as a verdict tool, at every value.

So: the TUI got its cluster-review surface (`g` from a cluster — groups
within one class, one verdict per group, cascade machine-labeled), and it
deliberately never crosses classes. The global listing survives as a
*viewing* surface; `accept/reject --cascade --across-classes` still works
but prints the measured numbers at use. `EmbedTask::Dedup` was measured
alongside and is not better than Document in the range that matters
(84.8% vs 83.6% at 0.90, worse elsewhere) — it stays wired to nothing.
The measurement is reproducible: `pkg calibrate-groups`.

## Migration sketch

1. Migration: `fact.tier` default `reviewed` for existing rows (they
   passed review or auto-accept under the old regime), `shadow` for new
   extraction output.
2. Extraction writes shadow facts directly; `fact_candidate` remains
   for the machine tiers' bookkeeping and the surfaced-verdict flow.
3. Ranking discount + provenance label in router/context/render.
4. Surfaced-verdict queue (small, trigger-driven) in TUI + MCP.
5. Utility report → ladder → gating, in that order, each with its own
   loud nightly line.
