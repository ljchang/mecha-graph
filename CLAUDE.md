## This repository is public, and is now the working copy

Until 2026-09-01 work happened in a private checkout and this repo was a
published artifact: `export-public.sh` stripped ten private files and ran the
denylist roster over the tree before anything left. That export is gone. Work
happens here now, which removes the only place the roster was ever checked —
so the gate had to move onto the repository itself.

**Two gates, and both fail closed.**

- `.githooks/pre-push` runs the roster before anything leaves the machine.
  Install it once: `git config core.hooksPath .githooks`. CI runs after the
  push, and a push to a public repository is already public — GitHub keeps
  unreachable objects reachable by SHA, so a force-push is not a delete. This
  hook is the only check that happens while a mistake is still local.
- `.github/workflows/denylist.yml` runs the same roster in CI, from the
  `PUBLIC_DENYLIST` secret.

**The roster is not in this repository and must never be.** It is a list of
the names and places that must not appear here, which makes it the most
sensitive file in the set. It lives at `~/.mecha-graph/denylist.txt` (0600)
and in the CI secret. The CI job never prints a matched term — GitHub masks a
secret's whole value in logs, not its individual lines, so echoing one would
publish the roster a finding at a time in a public build log. The hook, which
runs locally, does print it, because that is what makes a finding actionable.

**The roster has two kinds and the distinction is load-bearing.** `w` matches
at word boundaries, `p` as a substring. A first draft flattened them and
refused a clean tree over two ordinary English words that happen to contain a
roster term. The examples are deliberately not written out here: this file is
public, and naming them would put two roster entries in the repository the
roster exists to protect. LICENSE is excluded because it carries the copyright holder's name
by definition.

**What stays private:** the ten files `export-public.sh` used to strip — the
gold eval sets derived from real episodes, the operator docs, and the roster
tooling itself. They live in the private repo, which is not archived and is
still where those notes belong.

