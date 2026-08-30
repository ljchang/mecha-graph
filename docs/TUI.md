# The TUI

`mecha-graph tui` — keystroke-speed surfaces for the jobs a one-shot CLI is
bad at: review-queue triage, merge review, a search REPL with provenance
drill-down, quick capture, an entity browser with fact supersede, a task
board, and health stats.

## Global keys

```
Tab / Shift-Tab   cycle screens
1-7               jump straight to a screen (when nothing is being typed)
Esc               back out / empty the input buffer
q                 quit (when nothing is being typed; Esc-then-q works from anywhere)
Ctrl-Q            quit, even mid-typing
```

## The seven screens

### 1 · Review

The pending fact-candidate queue, opened on **clusters** — candidates
grouped by (proposer, predicate), each showing its class's acceptance
history ("✓ 41 / ✗ 3, 93% accepted on this class"), which is exactly the
evidence the autonomy ladder promotes on.

```
j/k        move            a      accept the cluster
Enter      inspect items   A      accept, creating new topic entities
p          proposer view   r/R    reject (R records a reason for the whole cluster)
s          shadow view     c / Esc  back
```

`s` opens the **surfaced-verdict view** (review-on-use): live shadow facts
that are about to matter — contradicting a reviewed fact (⚡), actually
served in a context pack, or spot-checked by a sampled class — at most ten
at a time. `y` confirms (tier → reviewed), `r` refutes as never true, `R`
refutes with a typed reason (it feeds rejection memory). Since extraction
mints shadow facts instead of queueing, this view is where most review now
happens; the candidate queue keeps only what cannot become a fact without
a human (commitments, flags, unresolvable subjects).

`g` on a cluster opens its **semantic groups** — the class's near-repeats
at the shared 0.83 floor, one verdict per group: `a` accepts, `r`/`R`
rejects, and in every case the leader is *your* verdict while members
cascade machine-labeled (one keystroke is one human verdict). Within-class
only, by measurement: same-class pairs carried the same human verdict ~89%
of the time; cross-class only ~63%, so crossing stays off this surface
(`pkg calibrate-groups` reproduces the numbers).

`p` rolls the queue up one level further — by proposing mechanism, with each
one's **human** accept rate and how much evidence it rests on (unjudged /
thin / some / solid). The pipeline's own dedup rejections are shown beside
the rate as "auto-dropped", never inside it, and a mechanism nobody has
judged shows a dash rather than 0%: "never reviewed" and "always rejected"
are opposite findings.

Inside a cluster: `a`/`r`/`Space` per item, `e` edits a candidate before
accepting (↑/↓ move field · Enter save · →/Ctrl-F complete an entity name).
Commitments materialize tasks — Enter reviews them individually.

### 2 · Merge

Duplicate-entity candidates, side by side.

```
j/k   move        ←/→   swap which side is kept
m     merge       s     skip
```

### 3 · Search

The REPL. Type to search; results carry provenance you can drill into.

```
#tag        filter to a tag          @source     browse one source
Enter       open the hit             Ctrl-E      semantic (embedding) search
Ctrl-P      toggle private tiers     /           lookup from a fact pane
```

An opened episode offers: `t` tag · `n` note · `m` link an entity ·
`p` cycle sensitivity tier · `e` edit · `d` delete — and a deleted episode
says so: **Ctrl-Z (or `mecha-graph undo`) restores**. Source-owned episodes
are not editable, and the screen says which.

### 4 · Capture

A quick note that saves as an episode with entities auto-linked (Enter);
Ctrl-T switches to capturing a fact instead.

### 5 · Entity

Lookup with suggestions (type · ↑/↓ pick · Enter opens). An entity shows
its facts and timeline:

```
j/k    move          s      supersede a fact (bi-temporal close + replacement)
Enter  follow        h/l    switch pane
/      lookup        —      timeline entries open their episode
y / u  confirm / refute an unreviewed (◌) fact in place
```

Facts nobody has vetted are marked `◌` — opening an entity is itself a
review trigger, so the verdict happens where the context is: `y` stands
behind the fact (tier → reviewed), `u` says it was never true. Reviewed
facts answer to `s` (supersede), not `u`: stopped-being-true and
never-was-true are different retractions.

### 6 · Tasks

The GTD board.

```
a      add            e      edit schedule
n/i/w/s/d/x           set status (next/inbox/waiting/scheduled/done/dropped)
Space  cycle status   z      show closed        Enter   page
```

### 7 · Stats

The same health numbers as `mecha-graph stats`, live.

## Where it fits

The TUI is the human half of the autonomy ladder: `precheck` drains the
mechanical part of the queue first, the Review screen's cluster verdicts
are what the per-class acceptance history is built from, and everything it
writes goes through the same store functions the CLI uses — there is
nothing the TUI can do that the [CLI](CLI.md) cannot, only faster hands.
