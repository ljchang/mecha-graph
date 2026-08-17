# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

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
