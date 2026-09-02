# Choosing an embedding model, and why the first two attempts measured nothing

2026-08-20. The store moved off `nomic-embed-text-v1.5` (Feb 2024, 137M, 768
dims) as part of consolidating mecha and mecha-graph onto one llama-server.
Since every vector had to be rewritten anyway, the model was up for
reconsideration at zero marginal migration cost.

The conclusion is one line — **`harrier-oss-v1-0.6b`, +33% MRR over the
incumbent** — and it is the least useful thing in this document. What took the
day was discovering that the two obvious ways to measure it both produced
confident numbers about the wrong thing.

## The candidates

| model | released | params | dims | context | licence |
|---|---|---|---|---|---|
| nomic-embed-text-v1.5 *(incumbent)* | Feb 2024 | 137M | 768 | 8k | Apache 2.0 |
| Qwen3-Embedding-0.6B | Jun 2025 | 600M | 1024 (MRL 32–1024) | 32k | Apache 2.0 |
| Qwen3-Embedding-4B | Jun 2025 | 4B | 2560 | 32k | Apache 2.0 |
| harrier-oss-v1-0.6b | Mar 2026 | 600M | 1024 | 32k | MIT |

Excluded after research: **EmbeddingGemma-300M** (2,048-token context is a
regression from 8k, and episodes clip at 8,000 chars); **nomic-embed-text-v2-moe**
(512-token limit); **Qwen3-VL-Embedding** (multimodal, and Qwen's own paper
puts it *below* same-size text-only models — 67.9 vs 70.58 MMTEB at 8B — while
llama.cpp support is an unmerged draft PR); and every hosted API, which the
local-first principle in INTEGRATIONS.md rules out regardless of quality.

All four were compared at **f16**. Quantising during a comparison measures
model × quant and cannot separate them — a model can lose because its
particular conversion is bad, which was the live risk for the community-built
Harrier GGUF.

## Attempt 1: the existing gold set. Measured nothing.

`~/.mecha-graph/eval/gold.jsonl` — 37 human-curated queries — was the obvious
instrument. Every
model scored identically:

| model | insight | memory | tasks |
|---|---|---|---|
| nomic *(baseline)* | 0.78 | 0.90 | 1.00 |
| qwen3-emb-0.6b | 0.78 | 0.90 | 1.00 |
| harrier-0.6b | 0.78 | 0.89 | 1.00 |
| qwen3-emb-4b | 0.78 | **0.87** | 1.00 |

Two reasons, and either alone is disqualifying:

**The queries are lexical.** `"conversations about furniture assembly"` targets
an episode containing "furniture assembly". BM25 answers it at rank 1.

**RRF hides the vector arm.** `hybrid_episodes` fuses a BM25 arm and a vector
arm with reciprocal rank fusion, which is rank-based — a rank-1 lexical hit
dominates whatever the vector arm returns. The embedder is masked by
construction.

The tell was visible before running: **`recall@10 = 1.00` on every job.** A
benchmark whose baseline is already perfect has no room to show an improvement,
and that should have been disqualifying rather than reassuring. The strongest
model on published benchmarks coming *last* is the other tell.

## Attempt 2: structural duplicate labels. Also measured nothing.

The second idea was to label duplicate pairs from structure — two facts sharing
`(subject, predicate, object)` are the same claim — and measure separation.
There were 59,202 such pairs. They are not the same claim:

One representative pair, generalised — the same `(subject, works_on, project)`
triple carrying two entirely different claims:

```
  "A fix for the timing bug in <project> is being deployed but requires testing."
  "<person> is developing <project>, an experimental platform for …"
```

**The triple does not determine the statement.** Restricting to
claim-determining predicates (`works_at`, `authored`, `family_of`, …) left 18
pairs with the same defect. This is a real property of the graph worth knowing
independently: the structured layer is a coarse index, the `statement` carries
the belief, and *no automatic non-circular source of duplicate labels exists in
this corpus*. The only oracle is a person.

(Using the pairs `precheck` already called duplicates would be circular — those
were selected by the incumbent model's own cosine.)

## Attempt 3: a semantic gold set. This one worked.

Build a set the embedder can actually fail: queries whose target shares **no
distinctive vocabulary** with them, so retrieval must be semantic.

- **Ground truth is free.** The label is the episode the query was written
  *from*, correct by construction. No human labelling.
- **A mechanical lexical gate does the real work.** A model told to avoid an
  episode's words reuses them constantly. Every generated query is rejected if
  it shares any content word with its source that is rare in the corpus
  (DF < 0.4%, ~82 of 20,444 episodes — a shared word that common still leaves
  80+ candidates to disambiguate). Without this gate the set drifts back into
  being a lexical benchmark *and looks like it is working*.
- **Rejections are recycled, not discarded.** Naming the leaked words back to
  the model and retrying once took the rejection rate from 78% to 7%.
- **Sample across the corpus, not the newest N.** The first attempt ordered by
  id and drew every target from one recent fortnight — ids 20415–20423 were
  consecutive turns of a single morning's conversation. Near-duplicate targets
  destroy the ground truth: a query written from one legitimately matches its
  neighbour.

Scoring is **vector-only** over a fixed 5,000-episode pool of real episodes
(seeded, so the pool cannot be resampled between arms). Not the production
path — deliberately. To compare embedders you must measure the embedder; the
integrated system is checked afterwards as a no-regression test, not as the
comparison. Each model family gets **its own prompt convention** (nomic's
`search_query:`/`search_document:`, Qwen and Harrier instructing the query side
only), or the comparison measures the prompt.

### Results — 80 queries, 5,000-episode pool, vector-only

| model | MRR | recall@1 | recall@10 | re-embed | store |
|---|---|---|---|---|---|
| nomic-embed-text *(incumbent)* | 0.2708 | 0.20 | 0.4375 | — | — |
| qwen3-emb-0.6b | 0.2846 | 0.20 | 0.425 | 10 min | 175 MB |
| **harrier-0.6b** | **0.3595** | **0.28** | **0.562** | **9 min** | **175 MB** |
| qwen3-emb-4b | 0.3878 | 0.30 | 0.562 | 27 min | 341 MB |

**Qwen3-Embedding-0.6B — the model MTEB pointed at — ties the incumbent.** Its
MTEB English v2 score (70.70) against nomic's ~62 predicted a wide margin. On
this corpus the gap vanished. Every source consulted said "test on your own
data"; this is what that warning is worth.

**Harrier beats Qwen3 decisively at identical size** (+26%), which also kills a
bias flagged *before* the run: queries were generated by qwen3.6, and a Qwen
embedder might have been favoured by Qwen-shaped phrasing. The opposite
happened. It is also evidence the community GGUF is sound — a broken conversion
produces garbage, not second place.

**The 4B's win over Harrier is not separable.** MRR gap 0.028 against a
standard error of roughly 0.03–0.04 at n=80, and **identical recall@10** — it
finds the same episodes and orders the top ten slightly better. Both are ~3 SE
clear of nomic. Paying 6.7× the model size and 3× the re-embed time for a
difference that cannot be measured is the wrong trade.

### What the failures are

44% of queries fail for every model, and reading them shows why:

The pattern, with the specifics generalised (the corpus is one person's life;
the figures and examples stay in the private checkout):

- A query describing a **recurring domestic situation** ranked its labelled
  episode at 276 — while the top hits were other episodes of the same
  recurring situation, any of which a person would have accepted.
- A query written from a **detail inside** an episode ranked it 33rd, behind
  episodes whose *topic* was that detail. The label was the worse answer.
- A query about a **research concept** ranked its labelled note 175th, while
  rank 2 was a different note squarely about that concept — arguably the
  better answer.

This is **sparse relevance judgment** — the classic failure of automatically
labelled retrieval sets. Exactly one episode is marked relevant, and the corpus
contains dozens of near-interchangeable domestic conversations and clusters of
same-day research notes. So `recall@10 = 0.562` is a **floor on real quality,
not an estimate of it**. The handicap is uniform across arms, so the *ranking*
is unaffected; only the absolute level is understated.

Two side findings: the generator sometimes wrote a query about a *detail*
rather than the episode's topic, which nothing could rank first; and the corpus's repetitiveness is the same
property that makes `precheck`'s dedup hard, and means retrieval returning five
near-identical episodes is low-value even when "correct".

## Threshold recalibration

`precheck::SEMANTIC_DUP_THRESHOLD` and `SEMANTIC_FLAG_THRESHOLD` encode **one
model's cosine scale**, and nothing about them said so. Measured on identical
text, same claim in different words versus unrelated text:

```
nomic-embed-text-v1.5   same 0.8650   unrelated 0.5579   gap 0.3071
Qwen3-Embedding-0.6B    same 0.6926   unrelated 0.2786   gap 0.4140
```

nomic's range is compressed — even unrelated text sits at 0.56. Carrying 0.93
onto a model whose genuine paraphrases score 0.69 would silently stop matching
anything.

Recalibrated by matching the **operating point** rather than the number:
sweeping `dedupe-facts` over the same corpus twice, once against the
pre-migration backup and once against the re-embedded store.

| threshold | nomic (backup) | harrier (live) |
|---|---|---|
| 0.99 | 23,226 | 13,160 |
| **0.97** | 24,010 | **29,200** ← matches nomic @ 0.93 |
| 0.95 | 24,619 | 32,362 |
| **0.93** | **29,273** | 37,344 |
| 0.83 | 85,910 | 82,566 ← matches nomic @ 0.83 |

`SEMANTIC_DUP_THRESHOLD` 0.93 → **0.97**; the flag threshold stays at 0.83.

**This preserves the rate, not the accuracy.** If 0.93 was mis-set for nomic,
0.97 is mis-set identically. Validating it needs labelled pairs, which
attempt 2 established this corpus cannot supply.

## What is written down where

- `~/.local/bin/mecha-embed-server` — the serving flags, and the numbers that
  chose the model. `--pooling last` is required for decoder-only embedders;
  `mean` silently produces plausible, worse vectors.
- `embed_meta` (migration 16) — which model produced the live vectors. Nothing
  about a vector reveals what made it, and a 768-dim nomic vector is
  indistinguishable from a truncated 768-dim Qwen one.
- `precheck.rs` — the calibration table, at the constants it explains.
- `docs/EMBEDDING-EXAMPLES.md` — the un-generalised versions of the examples
  above, verbatim as measured. **Private repo only and gitignored**, like
  `docs/OPERATIONS.md`. This document is the single source for both repos, so
  anything quoting an episode lives there instead of diverging into two copies.
- `eval/gold-semantic.jsonl` — the 80 queries, and `scripts/build-semantic-gold.py`
  / `score-semantic.py` / `diagnose-failures.py` that produce and read them.
  **In the private repo, not this one** — unlike the gold sets beside them,
  which now live at `~/.mecha-graph/eval/`. Generated from one person's
  episodes, so the queries are personal data even though the aggregate scores
  are not. This document keeps the method and the numbers; the examples above
  are generalised for the same reason mecha's `docs/MAIL-CORPUS-RESEARCH.md` is
  gitignored *in that repository* (`mecha/.gitignore`) — one corpus's contents
  belong to its owner. Named with its repo because a bare filename is not
  checkable from here: a reader who greps this repo's `.gitignore` for it
  finds nothing and reasonably concludes the claim is false.

## If this is redone

1. Check the instrument for a ceiling *and* a floor before trusting a number.
   A saturated baseline and a floored one are equally uninformative, and both
   produce clean-looking tables.
2. Measure the component, not the system, when choosing a component. Then
   re-check the system for regressions.
3. Give each model family its own prompt convention.
4. Sparse relevance judgment understates everyone. Do not read absolute
   retrieval numbers off a singly-labelled set.
5. The threshold that reads a model's output is part of the model choice, not
   a follow-up task.
