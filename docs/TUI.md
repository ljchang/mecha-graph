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
c / Esc    back            r/R    reject (R records a reason for the whole cluster)
```

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
```

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
