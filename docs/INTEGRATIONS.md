# Integrations: configuration & authentication

Every source and consumer, what it needs, and where credentials live.
Principle (spec §10): local-first — no credential here grants a cloud service
access to graph contents; auth only flows toward *fetching* your own data.

## The `mecha-graph source` CLI

Integrations are managed through a registry at `~/.mecha-graph/config.toml` (chmod 600):

```bash
mecha-graph source list                                # table: kind, enabled, auth state, last ok, items
mecha-graph source add ics --url '<secret-ical>' --me you@example.edu
mecha-graph source add slack --token xoxp-…            # validated via auth.test before saving
mecha-graph source add imessage --db ~/.mecha-graph/chat.db --self-handles '+16035550123'
mecha-graph source add mbox --path ~/Takeout/mail.mbox --me you@example.edu
mecha-graph source test [name]                         # auth/connectivity, no writes
mecha-graph source sync [name] [--full]                # ingest all enabled (cursored, idempotent)
mecha-graph source enable|disable|remove <name>
```

`add` runs the connectivity test before saving (`--no-test` to skip);
`sync` is what the nightly runs. `bee` and `sessions` self-register — they
need zero config.

## Status at a glance

| Integration | Direction | Auth | Status |
|---|---|---|---|
| Bee | source | `bee login` (token in Bee CLI config) | ✅ authenticated, synced nightly |
| Calendar (ICS) | source | secret iCal URL (capability URL) | ⚠️ one `mecha-graph source add ics --url …` away |
| Hermes sessions | source | none (local file, read-only) | ✅ |
| Claude Code sessions | source | none (local files) | ✅ |
| Slack | source | user token `xoxp-…` (or bot `xoxb-…`) | ✅ built — `mecha-graph source add slack --token …` |
| SMS / iMessage | source | synced copy of chat.db (Mac: Full Disk Access) | ✅ built — `mecha-graph source add imessage --db …` |
| Email (mbox) | source | none — mbox export (Gmail Takeout etc.) | ✅ built — `mecha-graph source add mbox --path …` |
| llama-server (embed + extract) | infra | none (localhost) | ✅ shared with mecha; :8080 chat, :8081 embed |
| Hermes (agent) | consumer | none (local stdio MCP) | ✅ wired |
| Claude Code (agent) | consumer | none (local stdio MCP) | ✅ wired |
| DuckDB analytics | consumer | none (reads the SQLite file) | ✅ |
| DB encryption | infra | local keyfile (auto) | ✅ SQLCipher, enabled 2026-08-02 |
| Email (live OAuth) | source | OAuth — lives in FlowMail/macOS | ⏳ FlowMail-side, by design |

## Sources

### Bee (ambient conversations)
- **Auth**: `bee login` (one-time browser flow). Check with `bee status`;
  re-auth with `bee logout && bee login`. Token is managed by the Bee CLI.
- **Auth needs a D-Bus session, which cron does not have.** The CLI keeps
  its token in the Secret Service keyring and reads it over the session
  bus, so under cron it fails with `Cannot autolaunch D-Bus without X11
  $DISPLAY` — a message that names neither the keyring nor the real fix.
  `nightly.sh` exports `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id
  -u)/bus`, which persists across logout/reboot only because lingering is
  enabled (`loginctl enable-linger $USER`); the keyring must also have
  been unlocked by a login at least once. This silently broke bee
  ingestion until 2026-08-13. Second time cron's thinner environment has
  hit this source — the first was `PATH` missing `~/.local/bin`. **Any
  source shelling out to a user-installed CLI needs its environment
  reconstructed explicitly**, and `mecha-graph stats` staleness is what catches
  it (that alert is the reason this was found).
- **Config**: `mode = "stream"` (current setup) — conversations + dailies are
  pulled straight from the Bee API (`bee … --json`) into the encrypted DB;
  the full JSON record is archived to `episode_raw`. No plaintext mirror
  exists (the historical `~/bee-sync` markdown mirror was removed 2026-08-02
  after 1:1 archive verification; mirror mode remains available by omitting
  `mode`).
- **Privacy**: episodes land as sensitivity `private` — excluded from default
  retrieval; opt in per query (`--private` / `include_private: true`).

### Calendar — the identity bridge (§5.1); highest-value config left to do
- **Auth**: a *secret iCal address* — a capability URL; anyone holding it can
  read your calendar, so it is treated as a credential.
  - Google: Calendar → Settings → (your calendar) → *Integrate calendar* →
    **Secret address in iCal format**. If leaked: *Reset* on that same page.
  - Outlook: Settings → Calendar → *Shared calendars* → Publish → ICS link.
- **Config**: put it in `~/.mecha-graph/nightly.env` (chmod 600, never in the repo):
  ```
  MECHA_GRAPH_ICS_URL=https://calendar.google.com/calendar/ical/.../basic.ics
  MECHA_GRAPH_SELF_EMAIL=you@example.edu
  ```
  (See docs/OPERATIONS.md — gitignored — for this machine's values.)
  The nightly fetches it to `~/.mecha-graph/calendar.ics` and ingests. Manual
  alternative: drop any exported `.ics` at `~/.mecha-graph/calendar.ics`, or run
  `pkg ingest ics <file> --me <your-email>` directly.
- Multiple calendars: add more `pkg ingest ics` lines in the nightly, or
  concatenate ICS files — events are idempotent by UID.

### Agent sessions (Hermes + Claude Code)
- **Auth**: none. `~/.hermes/state.db` is opened read-only;
  `~/.claude/projects/*/*.jsonl` are plain files. No writes ever.
- **Config**: paths overridable via `pkg ingest sessions --hermes/--claude`.

## Infrastructure

### llama-server (embeddings + Tier-7 extraction)

Replaced ollama on 2026-08-20. Not for tidiness: ollama runs its own
`llama-server` underneath, so the choice was never about the engine — only
about who sets the flags. Running both meant **two copies of the same 35B
model** resident at once (57.5 GB of a 121 GB unified pool), at a different
quantisation, under a chatml template override, with `--context-shift` on and
`--reasoning-budget` absent, so reasoning ran unbounded. What exactly caused
the 300 s extraction timeouts is **not established** — the measured
contributor is contention (two copies on one GPU put generation at 28 tok/s
against a 79.8 baseline); unbounded reasoning is plausible and was never
isolated.

- **Auth**: none — localhost.
- **Two endpoints, because llama-server holds one model per process:**
  - chat/extraction — `MECHA_GRAPH_CHAT_URL` or `[llm] base_url`,
    default `http://127.0.0.1:8080`
  - embeddings — `MECHA_GRAPH_EMBED_URL` or `[llm] embed_url`,
    default `http://127.0.0.1:8081`
- **Shared by default, never duplicated.** `Backend::resolve` probes
  `{base_url}/health`; if anything answers, it is used as-is. Installed beside
  mecha that is mecha's own server, holding the model once. There is
  deliberately no code that looks for mecha or reads its config —
  mecha-graph-core knows nothing about any agent (lib.rs rule 1), and a user
  running their own llama-server gets the shared path for the same reason.
- **Starting a server is opt-in, gated on `[llm] model_path`.** A machine that
  has not named a GGUF cannot start one, which is the whole safety property:
  probe-and-spawn on its own would answer a transient outage of mecha's server
  by loading a second 20 GB copy — silently, at 03:30, for the rest of the
  night. Nothing ever spawns at a URL that already answers, and a managed
  server is killed when the process exits.
- **The served model is discovered, not asserted.** llama-server ignores a
  request's `model` field, so `GET /props` → `model_alias` is what gets
  recorded in `extract_state.model`. A mismatch against an explicitly pinned
  `[llm] model` warns rather than refuses: a nightly that dies the first time
  you try a different model in the TUI would make the graph's health depend on
  remembering to edit a second config.
- **Embedding config**: `[llm] embed_model`, `embed_dims`, `embed_max_chars`.
  `embed::ensure_vec_dims` reconciles the `vec0` tables to `embed_dims` by
  rebuilding them — destructive by necessity, since vectors of a different
  width or model are not convertible — and `embed_meta` records
  (model, dims, instruction) so a swap is detectable afterwards.
  **Changing the embedder invalidates `precheck`'s thresholds**, which encode
  one model's cosine scale: measured on identical text, a same-claim pair
  scores 0.8650 on nomic and 0.6926 on Qwen3-Embedding-0.6B, against a
  `SEMANTIC_DUP_THRESHOLD` of 0.93. Recalibrate before re-enabling
  `precheck --auto-accept`.
- Serve an embedder with
  `llama-server -m <gguf> --port 8081 --embeddings --pooling last --embd-normalize 2`.

mecha's `docs/LLAMA-SERVER.md` is the full operational reference: slot
geometry, KV arithmetic, the measured `-np` table, and the request contract.

### Database & encryption
- DB: `~/.mecha-graph/graph.db` (override `MECHA_GRAPH_DB` or `--db`). Dir is chmod 700.
- **SQLCipher-encrypted at rest.** Key resolution on every open:
  `MECHA_GRAPH_DB_KEY` env → `MECHA_GRAPH_DB_KEYFILE` → a local keyfile (0600) →
  plaintext; see docs/OPERATIONS.md (gitignored) for this machine's
  values. `pkg encrypt` migrated the store in place with count
  verification; `pkg decrypt --out <path>` writes an ephemeral plaintext
  snapshot for DuckDB analytics.
- **Back up the keyfile separately from the DB file** (e.g. a password
  manager) — without it the graph is unrecoverable; with only it, an
  attacker still needs the DB file.
### Retention & streaming — the lifecycle of raw data

**Decision (2026-08-02, revised): stream where possible; capture-then-delete
where files are unavoidable. Plaintext residue trends to zero.**

Three retention modes per source (`--retention` on `mecha-graph source add`, or
`retention = "…"` in config.toml):

| Mode | What happens | When |
|---|---|---|
| `keep` (default) | files untouched | while building trust in extraction |
| `capture` | full raw archived to `episode_raw` *inside the encrypted DB*; files kept | transition |
| `capture_delete` | archived, then the plaintext file is deleted — only after the archive row is verified present | end state |

**Streaming beats all three when available** — plaintext never exists:
- **Bee**: `mode = "stream"` (current setup) pulls conversations + dailies
  straight from the Bee API (CLI `--json`); the full JSON record is always
  archived.
- **Calendar (URL)**: fetched and parsed in memory; no cache file.
- **Slack**: always streamed (API → DB).
- **iMessage / mbox**: inherently file-based (chat.db copy, Takeout export) —
  use `--retention capture_delete`; the transfer file is deleted after every
  episode's raw is archived, and the next sync re-creates it.

**Re-processing after deletion is guaranteed**: enrichment, embedding, and
Tier-7 extraction all read from the DB (`episode_raw` fallback where needed),
so prompt/schema improvements re-run against the archive — `pkg raw <uid>`
shows exactly what's preserved. `mecha-graph redact` deletes the archive row along
with everything else.

### At-rest architecture (final, 2026-08-02)

The design converged on **stream-first + encrypted archive**, which made a
separate encrypted vault unnecessary (a gocryptfs vault was built, then
removed before ever being used — see git history if it's ever wanted again):

1. **SQLCipher on `~/.mecha-graph/graph.db` is the at-rest layer.** It holds the
   distilled graph AND the full raw archive (`episode_raw`) for every
   streamed/captured episode — the DB is the system of record.
2. **No long-lived plaintext exists.** Bee streams from its API; calendar
   URLs parse in memory; Slack is API-native; iMessage/mbox transfer files
   are `capture_delete` (archived → verified → deleted).
3. **The keyfile is the single secret.** Back it up in your password
   manager, separate from any DB backup. Threat model, honestly stated:
   this protects against DB-file leaks (stray copies, backups); a thief who
   images the whole disk gets the keyfile too — the mitigation for that
   class is OS-level disk encryption (LUKS), a reinstall-level decision.
4. **Plaintext remnants can exist outside pkg**: agent session transcripts
   may quote graph content, and the distilled boot-context file (chmod 600)
   holds it by design. See docs/OPERATIONS.md (gitignored) for this
   machine's values.

Backups: copy `graph.db` (it's ciphertext at rest) + keep the keyfile in
the password manager. `pkg decrypt --out` produces plaintext snapshots for
DuckDB — treat those as ephemeral.

## Consumers (MCP)

The server is `pkg-mcp` — stdio transport, no network listener, no auth
surface; access = ability to execute the binary as you.

- **Hermes** — wired in `~/.hermes/config.yaml` under `mcp_servers.pkg`
  (backup kept alongside). Restart Hermes to pick it up.
- **Claude Code** — wired at user scope:
  `claude mcp add --scope user pkg -- ~/Github/personalized_knowledge_graph/target/release/mecha-graph-mcp`.
  Verify with `claude mcp list`; remove with `claude mcp remove pkg`.
- Any other MCP client: point it at the same binary, stdio transport.
- After `cargo build --release`, running servers keep the old binary until
  their host app restarts.

### Remote access — MCP over SSH (laptop → graph host)

The graph lives on one host; other machines get live access with zero local
state by running `pkg-mcp` through SSH (Tailscale authenticates). See
docs/OPERATIONS.md (gitignored) for this machine's values:

```bash
# on the laptop — verify the transport first:
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | \
  ssh -T -o LogLevel=ERROR examplehost \
  $HOME/Github/personalized_knowledge_graph/target/release/mecha-graph-mcp
# expect a single JSON line back ({"id":1,...serverInfo...})

# then register it:
claude mcp add --scope user pkg -- ssh -T -o LogLevel=ERROR examplehost \
  $HOME/Github/personalized_knowledge_graph/target/release/mecha-graph-mcp
```

Notes: `-T` + `LogLevel=ERROR` keep stdio clean (any motd/banner corrupts
JSON-RPC); use the absolute binary path (non-login shell, no PATH); writes
(`kg_upsert`) work identically — fact writes land in the graph host's staging
queue, episode writes (kind='episode') land as source-owned evidence whose
extracted facts stage on the next nightly.
Multiple simultaneous clients are fine (SQLite WAL + busy_timeout).
If full offline replicas are ever wanted instead, the uid-based `mecha-graph sync`
design is queued — the schema already carries sync identities.

### DuckDB
```sql
INSTALL sqlite; LOAD sqlite;
ATTACH '~/.mecha-graph/graph.db' AS pkg (TYPE sqlite);
```
Read-only analytics; never the system of record. (DuckDB wants a literal
path — see docs/OPERATIONS.md, gitignored, for this machine's values.)

## The credentialed sources in detail

### Slack
- **Get a token**: create an app at api.slack.com/apps → *OAuth & Permissions*
  → add **User Token Scopes**: `channels:history`, `groups:history`,
  `im:history`, `mpim:history`, `channels:read`, `groups:read`, `im:read`,
  `mpim:read`, `users:read`, `users:read.email` → *Install to Workspace* →
  copy the `xoxp-…` token. (A bot `xoxb-…` token also works but can't see
  your DMs or channels it isn't invited to.)
- `mecha-graph source add slack --token xoxp-…` — validated via `auth.test` first.
- What it does: `users.list` seeds every workspace member as a person with
  `slack_uid` **and** email identifiers (merges with calendar/email people
  deterministically); messages land one episode per channel-day. DMs are
  sensitivity `private`; channels `personal`.
- Tunables in config.toml: `max_channels` (default 50), `max_pages`/channel.
- Revoke: uninstall the app from the workspace, or rotate the token.

### SMS / iMessage
- **No API — a file**: sync a *copy* of the Mac's `chat.db` over Tailscale:
  `rsync mac:~/Library/Messages/chat.db ~/.mecha-graph/chat.db`
  (grant the Mac-side terminal Full Disk Access once; add the rsync to the
  nightly or a Mac-side launchd job). The DB is only ever opened read-only.
- `mecha-graph source add imessage --db ~/.mecha-graph/chat.db --self-handles '+1603…,you@x.com'`
- Identity: `handle.id` (E.164 phone or email) → deterministic
  `node_identifier`. Phone-only contacts get named after the number until a
  richer source supplies the real name — the identifier makes the merge
  automatic later. All episodes `private`.
- v1 limitation: messages whose body lives only in `attributedBody`
  (typedstream) rather than `text` are skipped.

### Email (mbox)
- **No credentials**: point at any mbox export — Gmail Takeout
  (takeout.google.com → Mail), Apple Mail export, mutt archives.
- `mecha-graph source add mbox --path ~/Takeout/mail.mbox --me you@example.edu`
- One episode per thread (References/In-Reply-To chains); bulk mail
  (List-Unsubscribe / List-Id / Precedence: bulk) is dropped at ingest (§5.3).
- Live sync remains FlowMail's job on macOS (it holds the Gmail/Outlook
  OAuth, spec §3); this path is for corpus backfill without new credentials.

## Credential hygiene

- `~/.mecha-graph/nightly.env` is chmod 600 and outside the repo; nothing secret is
  ever committed (`.gitignore` also excludes `*.db`).
- The ICS URL and the Bee token are the only two credentials in the pipeline
  today; both are revocable at their source (Google reset / `bee logout`).
- MCP server binds nothing: stdio only (§10's "loopback only" satisfied by
  not opening a socket at all).
