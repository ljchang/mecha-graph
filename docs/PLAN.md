# Self-improvement design

How the graph improves itself: the doctrine, the mechanism catalog, and the
build order. This is the decision layer over a longer internal research
record (not shipped in the public tree); where the research explored, this
file states what was chosen and why.

The objective: **automated self-improvement of the graph, at high accuracy,
with minimal owner oversight, using the principles by which humans do this
with each other.**

## Three doctrine decisions

### D1 — Autonomy posture: silent by default, escalate by exception

A review-everything posture maximizes owner decisions, which is the resource
the whole design economizes. Inverted:

- **Auto-accept from day one**: the high-trust rungs already write direct;
  durable predicates from allowlisted sources auto-accept; statistical
  co-occurrence stays direct-capped.
- **The review queue keeps only**: contradictions on single-valued
  predicates, identity merges (no undo — never automated), novel low-trust
  claims about people, anything above its slot's sensitivity ceiling.
- **Everything else rides the autonomy ladder**: staged → sampled
  (spot-check 1-in-10) → trusted; promote at 20 consecutive accepts ≥95%;
  demote on any in-use correction. Guardrails unchanged: evidence-rooted
  support, and irreversibles never graduate.
- **Owner interaction shapes**: in-flow corrections (free), pack flags
  (≤2 per pack, only when answer-changing), a weekly cluster digest instead
  of per-item cards, and rare `ask_owner` questions (ego-relations,
  significance, endorsement of a proposed rule).
- Budget target: **≤5 deliberate interactions per week** beyond in-flow
  corrections, instrumented via the event log before tightening further.

Safety is not the queue; it is reversibility + provenance + spot checks +
corrections demoting classes. That is the whole design.

### D2 — Graph-first; the interlock is satisfied by session-splitting

An agent harness's "web before memory" ordering (mecha's trifecta interlock)
is **exfiltration ordering inside one session** — after a private read,
outbound tools are gated, because a web query composed from private context
is a leak. It is *not* an epistemic claim that the web outranks the graph.
The flow that follows:

1. **Answer from the graph first, always.**
2. A gap or staleness flag → a query-log row (`status='gap'`).
3. A nightly research worker drains gaps in **fresh, untainted sessions**,
   seeded only by the user's own question text plus public entity names.
4. Findings land as `research:*` evidence episodes → extraction → ladder.

In-session web research is the exception, allowed only when the session has
not touched the graph and the question is plainly public.

### D3 — The error contract: who repairs what

One correction event, two independent consumers, attribution decided by
**what the context pack contained** — packs carry provenance, so the verdict
is mechanical:

| Pack state | Verdict | Who repairs |
|---|---|---|
| pack contained the wrong fact | **data error** | the graph |
| pack had the right fact; the answer ignored or misused it | **behavior error** | the agent |
| pack lacked the fact entirely | **gap** — nobody's fault | the research loop |

**The graph's duties on a data error** (all automatic): supersede the fact
(bi-temporal close); write the negation when the correction is a rejection;
demote the producing class on the ladder; run the blast-radius sweep
(predicate-scoped re-audit of that class's other output, landing as staged
candidates); log to the event log.

**The agent's duties**: its session-end distiller ships the correction as an
episode **always** (a `corrections: [{wrong, right, about}]` array in the
episode meta); its reflector mines a lesson **only** on a behavior-error
verdict. The agent's ruminations improve the agent; the graph's repairs
improve the data. Neither writes into the other's store — the episode is
the only crossing, and it crosses via `kg_upsert` like everything else.

## Gossip roles — structured asymmetry and verifiability

Adapted from the self-play literature (a Proposer/Solver/Judge split where
the Judge grades the *question*, and a discover → align → resolve →
**verify** pipeline). Five roles; the separation rule is the feedback-loop
guard generalized: **no role judges its own output.**

| Role | Does | Sees | Lives in |
|---|---|---|---|
| **Selector** | picks targets: demand × gap × λ-staleness | SQL only — no model | the graph (a query) |
| **Answerer A** | commits an answer *blind* | `scope=facts_only` | the agent, isolated context |
| **Answerer B** | commits an answer *blind* | `scope=evidence_only` | the agent, isolated context |
| **Verifier** | dereferences every provenance ref; checks each claim against the actual rows | both scopes, read-only | the agent; deterministic checks first, a model only for residue |
| **Judge** | grades the disagreement *and the question* — a template whose disagreements never survive verification gets retired | committed answers + verifier report | the agent |

This matters most because every claim's provenance ref is dereferenceable:
the Verifier can check "episode 4471 actually says that" mechanically.
Commit-then-reveal supplies the asymmetry; the role separation keeps it
through follow-up rounds.

## The self-improvement catalog

Every automated mechanism, in one place. "Autonomy" = where output lands;
wave = when to build.

| # | Mechanism | Trigger | Improves | Autonomy | Home | Wave |
|---|---|---|---|---|---|---|
| 1 | precheck tightening + cluster review + class promotions | queue state | oversight cost | owner, one-shot per class | graph | **1** |
| 2 | query-ledger quick wins: recurring-query rollups, materialized views | same query shape recurring | speed | full | graph | **1–2** |
| 3 | activation-based ranking arm + decay sweep | retrieval touches | speed, relevance | full | graph | 2 |
| 4 | pack flags (contradiction, staleness, thin, denial) | every retrieval | accuracy, oversight | surfacing only | graph detects, agent judges | 2 |
| 5 | corrections + blast-radius sweep (D3) | correction event | accuracy | auto supersede; sweep stages | both, per contract | 2 |
| 6 | negative facts | rejection | stops re-asking | full | graph | 2 |
| 7 | slot-targeted re-extraction | slot gap on a hot node | completeness | ladder | graph | 2 |
| 8 | linker additions: relation-aware beside alias-aware; typed closure templates | nightly | density | staged votes | graph | 2 |
| 9 | hand-written rules (3–5) + per-rule ledger | nightly over live facts | density, gaps | ladder | graph | 2 |
| 10 | Tier-1 gossip screening (roles above) | nightly | error detection, gaps | staged | agent | 3 |
| 11 | Tier-2 gossip sessions | hot-but-thin nodes; Tier-1 disagreements | **new relationships**, resolution | staged | agent | 3 |
| 12 | deferred web research worker (D2 flow) | query-log gaps | completeness, currency | staged + allowlist learning | agent | 3 |
| 13 | rule *mining* | graph bigger | density | rules themselves reviewed | graph | later |
| 14 | consolidation / link rewriting | nightly | density | staged | graph | later |

Gossip's place, stated once: it is the mechanism that **surfaces** issues
and gaps (rows 10–11); what it surfaces is then *filled* by the cheapest
capable route — link within the graph (8–9), re-extract from private
evidence (7), research the web (12), or ask the owner (last). Gossip finds;
the routes fill.

## Build waves

**Wave 1 — foundations.** No design risk, each useful alone: the
sensitivity-leak fix (`fact.sensitivity` = MAX over evidence — a privacy
bug, first regardless of everything else); the query log, retrieval-touch,
and event log (one migration — the demand signal every ranking rule
assumes, and the measurement substrate: "corrections per 100 retrievals
falls" is the north star); `mecha-graph fork` (an encrypted copy under a
fresh key — the test bed); precheck tightening plus
`mecha-graph review --clusters` plus the D1 auto-lanes.

**A note on provenance, since it carries the whole design.** The graph
already stores acquisition provenance (`fact.episode_id`, `extractor`).
`fact_observation` extends it to corroboration; a `kind` column covers
verification too:

    fact_observation(fact_id, episode_id, observed_at,
                     kind:  asserted | corroborated | verified
                          | disputed | corrected,
                     method: extractor | verifier-deref | gossip:tier1
                           | gossip:tier2 | research:web | user)

One table then answers "how do we know this, and how was it checked?" end
to end — it is the corroboration counter, the staleness clock, the
sensitivity MAX, the evidence-rooted-support guard, *and* the verification
audit trail. `kg_entity` should render it: "asserted by calendar 2026-03,
corroborated ×4, verified by provenance-deref 2026-08." That rendering is
also what makes spot-checking a sampled class fast.

**Wave 2 — the loop, no agents.** `fact_observation` with `kind` and
`method`; confidence that can fall; the evidence-rooted guard; node slots,
negatives, per-predicate λ, retrieval `scope`; pack flags; the class ledger
and ladder mechanics; D3 wiring on both sides; the relation-aware linker
and the first hand rules; the activation arm and recurring-query rollups.
Exit criterion: a month of event-log history showing interactions-per-week
and correction-rate trends.

**Wave 3 — the gossip agents.** First, the killer experiment on a fork
(count genuine disagreements versus model misreads — Tier-1 can still be
killed cheaply if the disagreements are noise); then Tier-1 screening with
the five roles; Tier-2 sessions (one entity per night, yield-metered); the
research worker on the D2 flow. Wave 3 is the density engine; Waves 1–2
are what keep it honest and cheap to supervise.

**Deliberately not building**: a federated profile (until a second *owner*
exists), KG embeddings, event-schema induction, rule mining before hand
rules prove the plumbing, and a second graph for any purpose short of a
second owner.

## Decided design points

Settled after the doctrine above, recorded here with their reasons.

**Ego-relations** (all multi-valued; a person can hold several at once —
colleague, friend, and collaborator co-exist as separate facts; none join
the single-valued contradiction list):

- `friend_of`, `family_of`, `colleague_of` — symmetric.
- `mentors` (inverse `mentored_by`) — directed, and it **consolidates**
  advising: `advises`/`advised_by` are predicate aliases of it. The
  mentee's career stage lives in `has_role`, not in the predicate — the
  relation survives promotions; only the role fact rolls over.
- `collaborates_with` — joint work: co-authorship or active unpublished
  projects.
- **Colleague scope is derived, not asserted**: shared `member_of`
  (dept/lab) ⇒ department colleague; shared `works_at` ⇒ university
  colleague; `colleague_of` with no shared affiliation ⇒ colleague in the
  field. No scoped predicate variants.
- Guard: `colleague_of` is **never auto-accepted** — having emailed someone
  does not make them a colleague.

**Confidence is a count-based Beta posterior over `fact_observation`.**
Priors (α, β) seed from the class ledger's per-(proposer, predicate)
acceptance history; Beta(1,1) for unseen classes. Observations with kind
`corroborated`/`verified` count for, `disputed`/`corrected` against,
distinct-episode-only. Stored confidence becomes a derived, recomputable
value — never a `MAX(confidence, new)` ratchet, which can only rise.

**λ values are hand-authored bands**, to be revisited from observed
supersession intervals once history exists (as half-lives):

| Half-life | Predicates |
|---|---|
| never (λ≈0) | `originated_in`, `attended`, `authored`, `family_of` |
| ~5 y | `friend_of`, `colleague_of`, `mentored_by` (historical) |
| ~3 y | `works_at`, `located_in` |
| ~2 y | `has_role`, `member_of`, `collaborates_with`, `mentors` (active) |
| ~1 y | `uses` |
| ~6 mo | `works_on`, `pursued_via` |
| ~1 mo | `assigned_to`, `waiting_on`, `blocked_by` |
| excluded | `mentions`, `about`, `related_to`, `discussed_at/during`, `organized` — evidence-anchored or ephemeral, never re-verified |

**Entity reads carry no sensitivity filter yet — instrumented, not
guessed.** Filtering the agent-facing read path before knowing what agent
paths actually need risks manufacturing gaps and errors; so it starts open,
and every private-derived fact served over MCP logs to the event log. The
eventual lock-down (the established allowlist pattern) gets made on data.

## Open questions

- The person slot-table rows (cut/add over the research draft).
- Wave-3 cadence: gossip sessions per night, and the Tier-1 nightly target
  count. Current lean: one entity per night, yield-metered.
