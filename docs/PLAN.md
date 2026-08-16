# Implementation plan — KG self-improvement (2026-08-12)

The decision layer over the internal research notes (not shipped in the
public tree). That file is the research
record and stays; this file resolves its conflicts, fixes three
doctrinal points, and sequences the build. When the two disagree,
this file wins.

The objective (unchanged): **automated self-improvement of the graph,
at high accuracy, with minimal owner oversight, using the principles
by which humans do this with each other.**

## Three doctrine decisions

### D1 — Autonomy posture: silent by default, escalate by exception

The old posture ("review-all until precision is observed") maximizes
owner decisions. Inverted, effective now:

- **Auto-accept day one**: rungs 1–4 already write direct; turn ON
  `--auto-accept durable` for allowlisted predicates/sources; NPMI
  stays direct-capped.
- **The review queue keeps only**: contradictions on single-valued
  predicates, identity merges (no undo — never automated), novel
  low-trust claims about people, anything above its slot's
  sensitivity ceiling.
- **Everything else rides the autonomy ladder**: staged → sampled
  (spot-check 1-in-10) → trusted, promote at 20 consecutive accepts
  ≥95%, demote on any in-use correction. Guardrails unchanged
  (evidence-rooted support; irreversibles never graduate).
- **Owner interaction shapes**: in-flow corrections (free), pack
  flags (≤2/pack, only when answer-changing), a **weekly cluster
  digest** instead of per-item cards, rare `ask_ada` questions
  (ego-relations, significance, why, rule endorsement).
- Budget target: **≤5 deliberate interactions/week** beyond in-flow
  corrections. Instrument it (event_log) before tightening further.

Safety is not the queue; it is reversibility + provenance + spot
checks + corrections demoting classes. That is the whole design.

### D2 — Graph-first; the interlock is satisfied by session-splitting

Correction of a confusing line in the research notes §4(a). mecha's "web
before memory" is **exfiltration ordering inside one session** —
after a private read, outbound tools are gated because a web query
composed from private context is a leak. It is *not* epistemic
priority of web over graph.

The flow that follows:

1. **Answer from the graph first, always.**
2. A gap or staleness flag → `query_log` row (`status='gap'`).
3. The nightly research worker drains gaps in **fresh, untainted
   sessions**, seeded only by the user's own question text + public
   entity names (or the approval lane).
4. Findings land as `research:*` evidence episodes → extraction →
   ladder.

In-session web research is the exception, allowed only when the
session has not touched pkg and the question is plainly public. The
"recognize currency questions before touching pkg" heuristic is
demoted to that exception path — it is no longer the primary design.

### D3 — The error contract: who ruminates about what

One correction event, two independent consumers, attribution decided
by **what the context pack contained** (packs carry provenance, so
this is mechanical):

| Pack state | Verdict | Who repairs |
|---|---|---|
| pack contained the wrong fact | **data error** | pkg |
| pack had the right fact; answer ignored/misused it | **behavior error** | mecha |
| pack lacked the fact entirely | **gap** — nobody's fault | research loop |

**pkg's duties on a data error** (all automatic): supersede the fact
(bi-temporal close); write the negation if the correction is a
rejection; **demote the producing class** on the ladder; run the
**blast-radius sweep** (predicate-scoped re-audit of that class's
other output → staged candidates); log to event_log.

**mecha's duties**: the session-end distiller ships the correction as
an episode **always** (with the `corrections: [{wrong, right, about}]`
array in meta — the one distiller change this plan requires); the
reflector mines a reflexion **only** on a behavior-error verdict.
mecha's ruminations improve mecha; pkg's repairs improve the data.
Neither writes into the other's store — the episode is the only
crossing, and it crosses via `kg_upsert` like everything else.

## Gossip roles — structured asymmetry and verifiability

Restored from the self-play research (Multi-Agent Evolve's
Proposer/Solver/Judge — the Judge grades the *question*; KARMA's
discover → align → resolve → **verify**). Five roles; the separation
rule is the feedback-loop guard generalized: **no role judges its own
output.**

| Role | Does | Sees | Lives in |
|---|---|---|---|
| **Selector** | picks targets: demand × gap × λ-staleness | SQL only — no model | pkg (a query) |
| **Answerer A** | commits an answer *blind* | `scope=facts_only` | mecha, isolated context |
| **Answerer B** | commits an answer *blind* | `scope=evidence_only` | mecha, isolated context |
| **Verifier** | dereferences every provenance ref; checks each claim against the actual rows | both scopes, read-only | mecha; deterministic checks first, model only for residue |
| **Judge** | bucket 2 vs bucket 3; grades the question (a template whose disagreements never survive verification gets retired) | committed answers + verifier report | mecha |

Why this matters most on a **shared** KG: every claim's provenance
ref is dereferenceable — the Verifier can check "episode 4471
actually says that" mechanically. That is verifiability the federated
profile cannot have (there the Verifier degrades to consistency
checks + the per-peer ledger, and hop attenuation prices what cannot
be verified). Commit-then-reveal supplies the asymmetry; the role
separation keeps it through the follow-up rounds.

## The self-improvement catalog

Every automated mechanism in the research record, in one place.
"Autonomy" = where output lands. Wave = when to build (below).

| # | Mechanism | Trigger | Improves | Autonomy | Home | Wave |
|---|---|---|---|---|---|---|
| 1 | precheck tightening + cluster review + class promotions | queue state | oversight cost | owner, one-shot per class | pkg | **1** |
| 2 | query-ledger quick wins: recurring-query rollups, materialized views | same query shape recurring | speed | full | pkg | **1–2** |
| 3 | ACT-R activation arm in ranking + §11.5 decay | retrieval_touch | speed, relevance | full | pkg | 2 |
| 4 | pack flags (contradiction, staleness, thin, denial) | every retrieval | accuracy, oversight | surfacing only | pkg detects, mecha judges | 2 |
| 5 | corrections + blast-radius sweep (D3) | correction event | accuracy | auto supersede; sweep stages | both, per contract | 2 |
| 6 | negative facts | rejection | stops re-asking | full | pkg | 2 |
| 7 | slot-targeted re-extraction (Extractable route) | slot gap on hot node | completeness | ladder | pkg | 2 |
| 8 | linker additions: RA beside AA; typed closure templates | nightly | density | staged votes | pkg | 2 |
| 9 | hand-written rules (3–5) + per-rule ledger | nightly over live facts | density, gaps | ladder | pkg | 2 |
| 10 | Tier-1 gossip screening (roles above) | nightly | error detection, gaps | staged | mecha | 3 |
| 11 | Tier-2 gossip sessions | hot-but-thin nodes; Tier-1 disagreements | **new relationships**, resolution | staged | mecha | 3 |
| 12 | deferred web research worker (D2 flow) | query_log gaps | completeness, currency | staged + allowlist learning | mecha | 3 |
| 13 | AMIE-style rule *mining* | graph bigger | density | rules themselves reviewed | pkg | later |
| 14 | consolidation / A-MEM link rewriting | nightly | density | staged | pkg | later |

Gossip's place in this catalog, stated once: it is the mechanism that
**surfaces** issues and gaps (rows 10–11); what it surfaces is then
*filled* by the cheapest capable route — link within the graph (8–9),
re-extract from private evidence (7), research the web (12), or ask
the owner (last). Gossip finds; the routes fill.

## Build waves

**Wave 1 — now.** No design risk, no blocking decisions, each useful
alone. (a) **Sensitivity leak fix** — `fact.sensitivity` = MAX over
evidence, `hybrid_facts` gains `include_private`, `summarize` uses
the allowlist form; a live privacy bug, first regardless of
everything else. (b) **query_log + retrieval_touch + event_log** —
one migration; the demand signal every ranking rule assumes and the
measurement substrate ("corrections per 100 retrievals falls" is the
north star). (c) **`pkg fork`** — encrypted copy, fresh key; the test
bed. (d) **precheck tightening + `mecha-graph review --clusters` + D1
auto-lanes** — drains the backlog in a handful of interactions and
flips the autonomy posture.

**A note on provenance, since it carries the whole design** (settled
2026-08-12: Ada wants the *how-known and how-verified* story
first-class). pkg already stores acquisition provenance
(`fact.episode_id`, `extractor`). `fact_observation` extends it to
corroboration; add a `kind` column and it covers verification too:

    fact_observation(fact_id, episode_id, observed_at,
                     kind:  asserted | corroborated | verified
                          | disputed | corrected,
                     method: extractor | verifier-deref | gossip:tier1
                           | gossip:tier2 | research:web | user)

One table then answers "how do we know this, and how was it checked?"
end to end — it is the corroboration counter, the staleness clock,
the sensitivity MAX, the evidence-rooted-support guard, *and* the
verification audit trail. `kg_entity` should render it: "asserted by
calendar 2026-03, corroborated ×4, verified by provenance-deref
2026-08". That rendering is also what makes spot-checking a sampled
class fast.

**Wave 2 — the loop, no agents.** `fact_observation` (with `kind` +
`method` as above) + confidence-can-fall + evidence-rooted guard; `node_slot` + negatives
+ `predicate.lambda` + retrieval `scope`; pack flags; class ledger +
ladder mechanics; D3 wiring (distiller `corrections` array on the
mecha side, precheck priority-routing on the pkg side); RA linker +
3–5 hand rules; ACT-R arm + recurring-query rollups. Exit criterion:
a month of event_log showing interactions/week and correction-rate
trends.

**Wave 3 — the gossip agents.** The killer experiment on a fork
(counts genuine disagreements vs model misreads — can still kill
Tier-1 cheaply); Tier-1 screening with the five roles; Tier-2
sessions (one entity/night, yield-metered); the research worker
rebuilt on the D2 flow. Wave 3 is the **density engine**; Waves 1–2
are what keep it honest and cheap to supervise.

**Deliberately not building**: the federated profile (until a second
*owner* exists), KG embeddings, KAIROS-style event schema induction,
rule *mining* before hand rules prove the plumbing, a second graph
for any purpose short of a second owner.

## Decisions actually blocking anything

Wave 1: **none** — that is the point of its composition.
Wave 2 needs: the ego-relation predicate list (Ada's taxonomy, no
literature can supply it); hand-authored λ values (~15) and slot
tables (~15 person + ~6 event); the confidence replacement rule
(recommend count-based posterior over `fact_observation`).
Wave 3 needs: gossip session cadence (recommend 1 entity/night,
yield-metered) and the Tier-1 nightly target count.

## Settled by Ada, 2026-08-12 — Wave 2 is unblocked

**Ego-relations** (all multi-valued; a person can hold several at
once — colleague + friend + collaborator co-exist as separate facts;
none join the single-valued contradiction list):

- `friend_of`, `family_of`, `colleague_of` — symmetric.
- `mentors` (inverse `mentored_by`) — directed; **consolidates
  advises/mentors** ("they are the same" — Ada). `advises` /
  `advised_by` become predicate aliases. The mentee's career stage
  (undergrad / grad student / postdoc) lives in `has_role`, NOT in
  the predicate — the relation survives promotions; only the role
  fact rolls over.
- `collaborates_with` (exists) — joint work: co-authorship or active
  not-yet-published projects.
- **Colleague scope is derived, not asserted**: shared `member_of`
  (dept/lab) ⇒ department colleague; shared `works_at` ⇒ university
  colleague; `colleague_of` with no shared affiliation ⇒ colleague
  in the field. No scoped predicate variants.
- Guard: `colleague_of` is **never auto-accepted** — having emailed
  someone does not make them a colleague.

**Confidence replacement rule — Option D**: count-based Beta
posterior over `fact_observation`. Prior (α, β) seeded from the
class ledger's per-(proposer, predicate) acceptance history
(the `review --clusters` priors); Beta(1,1) for unseen classes.
Observations with kind `corroborated`/`verified` count for,
`disputed`/`corrected` against; distinct-episode-only. Stored
confidence becomes a derived, recomputable value — no more
`MAX(confidence, new)` ratchet.

**λ values** — approved as drafted ("roughly right; tune if
needed"). Hand-authored; revisit from observed supersession
intervals once history exists. Bands (as half-lives):

| Half-life | Predicates |
|---|---|
| never (λ≈0) | `originated_in`, `attended`, `authored`, `family_of` |
| ~5 y | `friend_of`, `colleague_of`, `mentored_by` (historical mentors) |
| ~3 y | `works_at`, `located_in` |
| ~2 y | `has_role`, `member_of`, `collaborates_with`, `mentors` (active) |
| ~1 y | `uses` |
| ~6 mo | `works_on`, `pursued_via` |
| ~1 mo | `assigned_to`, `waiting_on`, `blocked_by` |
| excluded | `mentions`, `about`, `related_to`, `discussed_at/during`, `organized` — evidence-anchored or ephemeral, never re-verified |

**kg_entity / facts_for_node sensitivity — no filter for now.**
Ada's call: filtering risks manufacturing gaps and errors before we
know what the agent paths actually need; start open, lock down once
usage shows what's working. Do instrument it though: when the
agent-facing MCP path serves a private-derived fact, log to
event_log — so the eventual lock-down decision (the established
allowlist pattern, V008-style) is made on data, not guesswork.

Still open (minor): the slot-table row edit (cut/add over the
research-notes person table draft — no objections raised yet, so the
draft stands until Ada marks rows); Wave-3 cadence questions.
