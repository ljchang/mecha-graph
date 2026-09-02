//! Schema migrations. Same runner pattern as FlowMail's `db/migrations`, but the
//! schema starts clean at V001 implementing the spec (§4) directly — episode/mention
//! provenance, one bi-temporal `fact` table, and the productivity + context layers.

use crate::error::Result;
use rusqlite::Connection;

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial_schema",
        sql: V001_INITIAL_SCHEMA,
    },
    Migration {
        version: 2,
        name: "extract_state",
        sql: V002_EXTRACT_STATE,
    },
    Migration {
        version: 3,
        name: "episode_raw",
        sql: V003_EPISODE_RAW,
    },
    Migration {
        version: 4,
        name: "episode_annotation",
        sql: V004_EPISODE_ANNOTATION,
    },
    Migration {
        version: 5,
        name: "fts_porter_stemming",
        sql: V005_FTS_PORTER,
    },
    Migration {
        version: 6,
        name: "undo_log",
        sql: V006_UNDO_LOG,
    },
    Migration {
        version: 7,
        name: "episode_tombstone",
        sql: V007_EPISODE_TOMBSTONE,
    },
    Migration {
        version: 8,
        name: "fact_sensitivity",
        sql: V008_FACT_SENSITIVITY,
    },
    Migration {
        version: 9,
        name: "ledger",
        sql: V009_LEDGER,
    },
    Migration {
        version: 10,
        name: "fact_observation",
        sql: V010_FACT_OBSERVATION,
    },
    Migration {
        version: 11,
        name: "observation_confidence",
        sql: V011_OBSERVATION_CONFIDENCE,
    },
    Migration {
        version: 12,
        name: "class_ledger",
        sql: V012_CLASS_LEDGER,
    },
    Migration {
        version: 13,
        name: "slots_lambda_polarity",
        sql: V013_SLOTS_LAMBDA_POLARITY,
    },
    Migration {
        version: 14,
        name: "scholarly_acquaintance",
        sql: V014_SCHOLARLY_ACQUAINTANCE,
    },
    Migration {
        version: 15,
        name: "agent_verdict",
        sql: V015_AGENT_VERDICT,
    },
    Migration {
        version: 16,
        name: "embed_meta",
        sql: V016_EMBED_META,
    },
    Migration {
        version: 17,
        name: "candidate_reviewed_by",
        sql: V017_CANDIDATE_REVIEWED_BY,
    },
    Migration {
        version: 18,
        name: "entity_proposal",
        sql: V018_ENTITY_PROPOSAL,
    },
    Migration {
        version: 19,
        name: "unlinked_mention",
        sql: V019_UNLINKED_MENTION,
    },
    Migration {
        version: 20,
        name: "agent_node",
        sql: V020_AGENT_NODE,
    },
    Migration {
        version: 21,
        name: "fact_tier",
        sql: V021_FACT_TIER,
    },
    Migration {
        version: 22,
        name: "vec_rejected",
        sql: V022_VEC_REJECTED,
    },
    Migration {
        version: 23,
        name: "candidate_embedding",
        sql: V023_CANDIDATE_EMBEDDING,
    },
    Migration {
        version: 24,
        name: "cooccurrence_alarm",
        sql: V024_COOCCURRENCE_ALARM,
    },
    Migration {
        version: 25,
        name: "cooccurrence_alarm_first_observed",
        sql: V025_COOCCURRENCE_ALARM_FIRST_OBSERVED,
    },
];

/// Semantic rejection memory (review-on-use §5): the embedded index of
/// human-rejected statements.
///
/// Precheck's rejection memory was exact-normalized-string only, and the
/// mid-August embedding-model swap guarantees paraphrase leaks — the same
/// wrong claim re-extracted in different words re-claims the owner's
/// attention. This table holds one vector per HUMAN-rejected candidate
/// (machine rejects are excluded for the same reason they are excluded
/// everywhere: a lane must not feed the memory that judges its own
/// input), populated incrementally by `pkg embed`, compared by precheck
/// at the same 0.97 threshold the live-fact dedup earned.
///
/// Created at the compiled-in default width like its V001 siblings; an
/// embedding-model change rebuilds all three through
/// `embed::ensure_vec_dims`, and `embed::ensure_vec_rejected` re-aligns
/// this one on a store whose vectors were rebuilt before V022 existed.
/// The index is a derivable cache — dropping it loses nothing that one
/// `pkg embed` cannot restore.
const V022_VEC_REJECTED: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS vec_rejected USING vec0(candidate_id INTEGER PRIMARY KEY, embedding FLOAT[768]);
"#;

/// The review queue's vectors, kept between runs.
///
/// Grouping the pending queue by similarity re-embedded every pending
/// statement on every call: ~7,000 statements, measured at 40 s for the
/// cross-class layer and budgeted at 360 s by the web route. The vectors
/// were thrown away when the process exited, so the same statements were
/// embedded again on the next call — for the next threshold the stepper
/// visited, for the class listing after the global one, for the TUI after
/// the phone. The reported cost was not the wait itself but what it did to
/// a sitting: entering a group to judge its members individually and
/// stepping back out re-ran the whole thing, so the ordinary loop of
/// curation was the expensive one and the queue stopped getting cleared.
///
/// A pending statement's text does not change while it waits, so its vector
/// is immutable and re-deriving it is pure waste. Only genuinely new
/// candidates need the embedding server.
///
/// **A plain table, not a `vec0` virtual one**, unlike its three siblings.
/// Those exist to be *searched* — kNN over the whole store — and pay a
/// vec0 index for it. Nothing searches this one: the grouping fetches the
/// vectors for a known set of ids and does its own in-process greedy
/// clustering, so this is a lookup by primary key and a plain table is both
/// the simpler and the faster answer. It also lets the row carry its own
/// identity, which is the part that makes it safe.
///
/// `text_hash` is over the model, the embed task's instruction identity and
/// the exact text embedded — everything that decides what the numbers mean.
/// A row whose hash does not match what is about to be embedded is a miss,
/// so swapping the embedding model, changing an instruction, or editing a
/// statement all invalidate by construction rather than by remembering to
/// write an invalidation rule. That is the same reasoning `embed_meta`
/// records for the searchable tables, moved into the key so nothing has to
/// consult it. `dims` rides along so a mismatch is legible in the table
/// rather than only inside sqlite-vec's error text.
///
/// `embedding` is a BLOB of little-endian `f32`, not the JSON text the vec0
/// tables are fed. Nothing here parses as a vector on sqlite's behalf, so the
/// format is this table's own choice, and the queue is large enough for the
/// choice to matter: 768 floats are ~3 KB raw and ~9 KB as JSON, which across
/// a seven-thousand-candidate queue is the difference between about twenty
/// megabytes and about sixty. `dims` makes a truncated row legible without
/// reading it.
///
/// Derivable, like `vec_rejected`: dropping it costs one slow grouping and
/// nothing else, which is why it carries no foreign key and is pruned of
/// candidates that have left the queue rather than migrated with them.
const V023_CANDIDATE_EMBEDDING: &str = r#"
CREATE TABLE IF NOT EXISTS candidate_embedding (
    candidate_id INTEGER PRIMARY KEY,
    text_hash    TEXT NOT NULL,
    dims         INTEGER NOT NULL,
    embedding    BLOB NOT NULL,
    written_at   TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// Review-on-use (docs/REVIEW-ON-USE.md): facts get a `tier`.
///
/// The review model inverts: the human is the scarcest resource and
/// retrieval is the only ground truth of usefulness, so extraction output
/// stops queueing for review-at-birth and lands as a *shadow* fact —
/// retrievable, rank-discounted, labeled unreviewed — that earns a human
/// verdict only when it is about to matter. `tier` is `'reviewed'` |
/// `'shadow'`; readers treat anything other than `'reviewed'` as shadow,
/// so an unknown value written by a later version degrades to the safe
/// reading (unreviewed) instead of impersonating a vetted fact.
///
/// Existing rows backfill `'reviewed'` via the DEFAULT: under the old
/// regime every fact row had passed human review, an auto-accept lane the
/// ladder had earned, or a deterministic extractor — the old queue WAS the
/// review. The same DEFAULT keeps every direct `assert_fact` caller
/// (linkers, ics, gtd, corrections, the owner's CLI) born-reviewed; only
/// the shadow mint path writes `'shadow'`.
///
/// `fact_candidate.fact_uid` links a candidate to the fact it minted, so a
/// later human verdict on the *fact* can settle the *candidate* — which is
/// what keeps `HUMAN_VERDICT_SQL` and the ladder honest: a shadow row
/// counts as no verdict at all until a human confirms or refutes it.
const V021_FACT_TIER: &str = r#"
ALTER TABLE fact ADD COLUMN tier TEXT NOT NULL DEFAULT 'reviewed';
ALTER TABLE fact_candidate ADD COLUMN fact_uid TEXT;

-- The surfacing queries walk shadow rows only; reviewed rows never match.
CREATE INDEX IF NOT EXISTS idx_fact_shadow ON fact(tier) WHERE tier <> 'reviewed';
CREATE INDEX IF NOT EXISTS idx_candidate_fact_uid ON fact_candidate(fact_uid)
 WHERE fact_uid IS NOT NULL;
"#;

/// The agent as something a task can wait on.
///
/// `waiting_on` was seeded as "Task is waiting on Person" and the description
/// is prose rather than a constraint, so this widens the sentence rather than
/// the schema — but the sentence is what a reader and an extractor go by, and
/// leaving it saying Person while the harness writes an agent into it is how
/// documentation stops being true.
///
/// The node is seeded here rather than created on demand because
/// `set_task_waiting_on` refuses a name the graph does not already know —
/// that refusal is the typo protection `create_task` has for projects, and an
/// agent node minted on first use would punch a hole straight through it.
const V020_AGENT_NODE: &str = r#"
UPDATE predicate SET description = 'Task is waiting on Person or Agent'
 WHERE name = 'waiting_on';

INSERT OR IGNORE INTO nodes (id, node_type, name, canonical_name, source, confidence)
VALUES ('agent-mecha', 'agent', 'mecha', 'mecha', 'system', 1.0);
"#;

/// Weak alias matches that were refused for want of corroboration.
///
/// The linker used to commit every unambiguous alias match, which is how a
/// student's first-name alias collected a thousand mentions of somebody
/// else's toddler. Refusing an uncorroborated match is the fix; **recording
/// what was refused is what makes the refusal useful**, because a bare
/// first name that keeps appearing and can never be corroborated is not
/// noise — it is a person the graph has no node for. That is exactly how
/// Wren went unnoticed for years: mentioned constantly, with nothing to
/// attach the mentions to and nothing anywhere saying so.
///
/// Keyed `(alias, episode_id)` so re-linking an episode is idempotent.
const V019_UNLINKED_MENTION: &str = r#"
CREATE TABLE IF NOT EXISTS unlinked_mention (
    alias      TEXT    NOT NULL,
    node_id    TEXT    NOT NULL REFERENCES nodes(id)   ON DELETE CASCADE,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    at         TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (alias, episode_id)
);
CREATE INDEX IF NOT EXISTS idx_unlinked_alias ON unlinked_mention(alias);
"#;

/// Entity maintenance proposals — the queue the fact layer has always had
/// and the entity layer never did.
///
/// Facts get proposed by a named class, queued, reviewed, and promoted up
/// the autonomy ladder on their human accept rate. Entities got nothing:
/// creating, renaming, merging, splitting and retyping were all hand
/// surgery, which is why a first-name alias could quietly move one person's
/// decade onto another person's node and go unnoticed for three years.
///
/// `detector` is the class, deliberately named the same way `(proposer,
/// predicate)` is, so this queue can ride the same ladder later without a
/// second notion of what a class is.
///
/// **A decided proposal is never re-proposed**, which is what the unique
/// index buys: the nightly re-runs every detector over the whole graph and
/// `INSERT OR IGNORE` skips anything already on file, accepted or rejected.
/// A rejection is therefore durable — the same reasoning as mecha's retired
/// rules, where re-deriving a refused change means the refusal was never
/// paid for. `other_id` is '' rather than NULL so the index can be plain.
const V018_ENTITY_PROPOSAL: &str = r#"
CREATE TABLE IF NOT EXISTS entity_proposal (
    id          INTEGER PRIMARY KEY,
    detector    TEXT NOT NULL,              -- the class: email_named_person, near_duplicate_person, …
    kind        TEXT NOT NULL,              -- merge|retype|rename|reattribute|review
    subject_id  TEXT NOT NULL,              -- the node this is about
    other_id    TEXT NOT NULL DEFAULT '',   -- the second node, for merge/reattribute ('' = none)
    payload     TEXT,                       -- JSON: {"to_type":"org"} / {"new_name":"…"}
    evidence    TEXT NOT NULL,              -- why, in words a person can act on
    score       REAL,                       -- how strong, for ordering
    status      TEXT NOT NULL DEFAULT 'pending',  -- pending|accepted|rejected
    created_at  TEXT NOT NULL DEFAULT (datetime('now')),
    decided_at  TEXT,
    decided_by  TEXT                        -- 'user' | 'auto'
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_entity_proposal_unique
    ON entity_proposal(detector, kind, subject_id, other_id);
CREATE INDEX IF NOT EXISTS idx_entity_proposal_status
    ON entity_proposal(status, score DESC);
"#;

/// Who decided a candidate: 'user', 'auto' (precheck's lanes), or
/// 'cascade:<seed id>' (a similarity cascade fanned out from one human
/// verdict).
///
/// Added 2026-08-23, when the similarity cascade made the gap unaffordable:
/// an accepted row carried no record of who accepted it, so every accept —
/// the durable lane's, the ladder's, a cascade's — counted toward the
/// "human" rate that promotes classes. Machine *rejects* were already
/// excluded by their `precheck:%` reason; accepts had no equivalent, which
/// is the same contamination the cluster view was caught displaying on
/// 2026-08-22, running in the direction that widens autonomy instead of
/// narrowing it. A cascade writing dozens of rows per keystroke would have
/// promoted classes on their own volume.
///
/// NULL means pre-migration: those rows keep counting exactly as they did
/// (accepts as the owner's, rejects filtered by reason), because zeroing
/// the owner's verdict history would gut the record the ladder runs on.
/// The contamination stops growing; it is not rewritten.
const V017_CANDIDATE_REVIEWED_BY: &str = r#"
ALTER TABLE fact_candidate ADD COLUMN reviewed_by TEXT;
"#;

const V015_AGENT_VERDICT: &str = r#"
-- What an agent mechanism concluded about a pending candidate.
--
-- NOT a decision. `fact_candidate.status` stays exactly where it was; this
-- records an opinion beside it. The distinction is the whole point: a
-- mechanism has to be able to be wrong in public for long enough to be
-- measured, and a store that promoted its own verdicts would erase the
-- record that made the measurement possible.
--
-- This is the missing rung of the autonomy ladder. Without somewhere for
-- verdicts to accumulate, every run starts from zero, precision can never
-- be sampled against the owner's own judgement, and no class can ever
-- graduate from `staged`.
CREATE TABLE IF NOT EXISTS agent_verdict (
    id            INTEGER PRIMARY KEY,
    candidate_id  INTEGER NOT NULL REFERENCES fact_candidate(id) ON DELETE CASCADE,
    -- Which dialogue produced it: corroboration|persistence|resolution.
    -- Kept open rather than CHECKed, because the patterns are still being
    -- discovered and a migration is a bad place to freeze a taxonomy.
    mechanism     TEXT NOT NULL,
    verdict       TEXT NOT NULL,
    basis         TEXT,
    -- The model that produced it. A verdict from a 35B local model and one
    -- from a frontier model are not the same evidence, and a ledger that
    -- forgets which it saw cannot calibrate either.
    model         TEXT,
    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
    -- What the human decided afterwards, written when the candidate is
    -- reviewed. NULL means not yet scored — this column is what turns a
    -- pile of opinions into a precision measurement.
    outcome       TEXT
);

-- One mechanism's latest word per candidate is what matters; the history
-- is kept, so no UNIQUE here, only the lookup path.
CREATE INDEX IF NOT EXISTS idx_agent_verdict_candidate
    ON agent_verdict(candidate_id, mechanism);
CREATE INDEX IF NOT EXISTS idx_agent_verdict_scoring
    ON agent_verdict(mechanism, verdict, outcome);
"#;

const V014_SCHOLARLY_ACQUAINTANCE: &str = r#"
-- People you know through their WORK, not through a personal tie.
-- The settled ego-relations all presume one — friend, family, colleague,
-- mentor, collaborator — so the commonest academic case had nowhere to
-- go: you watched someone give a talk, took notes on their research, and
-- have no relationship with them at all.
--
-- Three predicates, because three different things were being conflated:

-- 1. What someone's research is ABOUT. Distinct from works_on, and the
-- reason is λ, not taste: works_on runs at 1.39 (~6mo) because projects
-- turn over fast, so recording a research programme as works_on would
-- have the decay sweep calling it stale within two quarters. A research
-- area is one of the most stable things about an academic — ~5y band.
INSERT OR IGNORE INTO predicate (name, inverse, description, lambda) VALUES
    ('researches', NULL,
     'Person studies this topic/area — their research programme, not a project', 0.14),

-- 2. The act of presenting. Evidence-anchored like attended/authored: a
-- talk given in 2019 does not stop having been given, so λ = 0 (never
-- re-verified). Object is the talk, venue or subject.
    ('presented', NULL,
     'Person gave a talk/poster/seminar on this (an event that happened)', 0.0),

-- 3. The ego-relation itself: I know OF them. Weakest tie in the
-- taxonomy and deliberately so — it asserts acquaintance with someone's
-- work, not with the person. λ = 0: having encountered their work never
-- becomes false. Left OUT of NEVER_AUTO (precheck.rs) on purpose — that
-- guard exists because emailing someone does not make them a colleague,
-- a claim about social standing; "I saw this person speak" is a claim
-- about a room I was in, and holding it for review would tax exactly the
-- note-taking this is meant to capture.
    ('knows_of', NULL,
     'Ego: user knows of this person through their public work, with no personal tie', 0.0);

INSERT OR IGNORE INTO predicate_alias (alias, name) VALUES
    ('studies',        'researches'),
    ('research_area',  'researches'),
    ('works_in',       'researches'),
    ('gave_talk',      'presented'),
    ('spoke_on',       'presented'),
    ('presented_at',   'presented'),
    ('heard_of',       'knows_of'),
    ('aware_of',       'knows_of');

-- Talks are a route to the how_known slot, which until now only social
-- relations could fill. Recorded as a slot row so goal-1 probing can see
-- that "how do I know this person" has an answer of this kind.
INSERT OR IGNORE INTO node_slot
    (node_type, slot, kind, predicate, cardinality, route, ceiling, required) VALUES
    ('person', 'research', 'predicate', 'researches', '0-n', 'extractable', 'private', 0);
"#;

const V013_SLOTS_LAMBDA_POLARITY: &str = r#"
-- Wave 2 schema (PLAN.md, decisions settled 2026-08-12):
-- ego-relation predicates, per-predicate change rates (λ), negative
-- facts (polarity), and the slot tables that define completeness.

-- 1. Ego-relations (the owner's taxonomy). All multi-valued; colleague
-- scope (dept/university/field) is DERIVED from shared affiliations,
-- never predicate variants. advises consolidates into mentors
-- ("they are the same"); career stage lives in has_role.
-- NOTE: advised_by/mentored_by are the INVERSE direction — they can't
-- be aliases (an alias can't swap subject and object); the extraction
-- vocabulary must emit the mentor-first direction.
ALTER TABLE predicate ADD COLUMN lambda REAL;  -- change rate /year; NULL = not re-verified

INSERT OR IGNORE INTO predicate (name, inverse, description, lambda) VALUES
    ('friend_of',    'friend_of',    'Person is a friend of Person (symmetric)',            0.14),
    ('family_of',    'family_of',    'Person is family of Person (symmetric)',              0.0),
    ('colleague_of', 'colleague_of', 'Person is a colleague of Person (symmetric; scope derived from shared affiliations)', 0.14),
    ('mentors',      'mentored_by',  'Person mentors/advises Person (career stage lives in has_role)', 0.35),
    ('has_role',     'role_of',      'Person holds role/title (single-valued live)',        0.35);
INSERT OR IGNORE INTO predicate_alias (alias, name) VALUES
    ('advises',      'mentors'),
    ('friends_with', 'friend_of'),
    ('colleague',    'colleague_of'),
    ('role',         'has_role'),
    ('title',        'has_role');

-- 2. λ values (approved bands, as per-year rates: ln2 / half-life).
-- never → 0 · ~5y → 0.14 · ~3y → 0.23 · ~2y → 0.35 · ~1y → 0.69 ·
-- ~6mo → 1.39 · ~1mo → 8.3. NULL predicates (mentions, about,
-- related_to, discussed_*, organized, GTD structure) are
-- evidence-anchored or structural — never re-verified.
-- originated_in predates V013 (V001: also used for task/fact→episode
-- provenance; the person-origin slot reuses it toward places).
UPDATE predicate SET lambda = 0.0  WHERE name IN ('attended','authored','originated_in');
UPDATE predicate SET lambda = 0.23 WHERE name IN ('works_at','located_in');
UPDATE predicate SET lambda = 0.35 WHERE name IN ('member_of','collaborates_with');
UPDATE predicate SET lambda = 0.69 WHERE name IN ('uses');
UPDATE predicate SET lambda = 1.39 WHERE name IN ('works_on','pursued_via');
UPDATE predicate SET lambda = 8.3  WHERE name IN ('assigned_to','waiting_on','blocked_by');

-- 3. Negative facts (mechanism #6: rejection memory — stops re-asking).
-- fact_current is redefined to positive-only: every existing consumer
-- (edges/graph traversal, linkers, GTD, stats, contradiction checks)
-- means "current positive beliefs" by it — a negative edge in graph
-- traversal would be a bug. Negatives are queried explicitly from
-- `fact`; their statements carry the negation in text.
ALTER TABLE fact ADD COLUMN polarity TEXT NOT NULL DEFAULT 'positive'
    CHECK (polarity IN ('positive','negative'));
-- One live fact per (subject, predicate, object) AND polarity — a live
-- negation may coexist with a live positive (contested state is real
-- state; the contradiction machinery surfaces it, a human resolves it).
DROP INDEX IF EXISTS idx_fact_live;
CREATE UNIQUE INDEX idx_fact_live ON fact(subject_id, predicate, object_id, polarity)
    WHERE valid_to IS NULL AND invalidated_at IS NULL;
DROP VIEW IF EXISTS fact_current;
CREATE VIEW fact_current AS
    SELECT * FROM fact
    WHERE valid_to IS NULL AND invalidated_at IS NULL AND polarity = 'positive';

-- 4. Slot tables: what "complete" means per node type (goal-1 probing
-- reads this; kind: predicate | identifier | ego | derived).
CREATE TABLE IF NOT EXISTS node_slot (
    node_type   TEXT NOT NULL,
    slot        TEXT NOT NULL,
    kind        TEXT NOT NULL DEFAULT 'predicate',
    predicate   TEXT,              -- canonical predicate / identifier kind; NULL for ego/derived
    cardinality TEXT,              -- '0-1' | '0-n' | '>=1'
    route       TEXT,              -- extractable | external | derivable
    ceiling     TEXT NOT NULL DEFAULT 'private',
    required    INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (node_type, slot)
) WITHOUT ROWID;

INSERT OR IGNORE INTO node_slot
    (node_type, slot, kind, predicate, cardinality, route, ceiling, required) VALUES
    ('person', 'email',        'identifier', 'email',             '>=1', 'extractable', 'private', 1),
    ('person', 'phone',        'identifier', 'phone',             '0-n', 'extractable', 'private', 0),
    ('person', 'website',      'identifier', 'url',               '0-n', 'external',    'private', 0),
    ('person', 'github',       'identifier', 'handle',            '0-1', 'external',    'private', 0),
    ('person', 'orcid',        'identifier', 'orcid',             '0-1', 'external',    'private', 0),
    ('person', 'employer',     'predicate',  'works_at',          '0-1', 'extractable', 'private', 1),
    ('person', 'role',         'predicate',  'has_role',          '0-1', 'extractable', 'private', 1),
    ('person', 'origin',       'predicate',  'originated_in',     '0-1', 'external',    'private', 0),
    ('person', 'education',    'predicate',  'attended',          '0-n', 'external',    'private', 0),
    ('person', 'how_known',    'ego',        NULL,                '>=1', 'extractable', 'private', 1),
    ('person', 'closeness',    'derived',    NULL,                '0-1', 'derivable',   'private', 0),
    ('person', 'collaborators','predicate',  'collaborates_with', '0-n', 'derivable',   'private', 0),
    ('person', 'projects',     'predicate',  'works_on',          '0-n', 'extractable', 'private', 0),
    ('person', 'groups',       'predicate',  'member_of',         '0-n', 'extractable', 'private', 0),
    ('person', 'publications', 'predicate',  'authored',          '0-n', 'external',    'public',  0),
    ('event',  'participants', 'predicate',  'attended',          '>=1', 'extractable', 'private', 1),
    ('event',  'organizer',    'predicate',  'organized',         '0-1', 'extractable', 'private', 0),
    ('event',  'place',        'predicate',  'located_in',        '0-1', 'extractable', 'private', 0),
    ('event',  'topic',        'predicate',  'about',             '0-n', 'extractable', 'private', 1),
    ('event',  'series',       'derived',    NULL,                '0-1', 'derivable',   'private', 0),
    ('event',  'outcome',      'derived',    NULL,                '0-1', 'extractable', 'private', 0);
"#;

const V012_CLASS_LEDGER: &str = r#"
-- The autonomy ladder's state (PLAN.md D1): per-(proposer, predicate)
-- class rung + consecutive-human-accept streak. Accept/reject TOTALS
-- stay derived from fact_candidate (single source of truth — the same
-- history `review --clusters` and the confidence prior read); only the
-- rung and the streak are state, because demotions are events.
CREATE TABLE IF NOT EXISTS class_ledger (
    proposer    TEXT NOT NULL,
    predicate   TEXT NOT NULL,   -- raw payload predicate or '(kind)', the cluster-view key
    rung        TEXT NOT NULL DEFAULT 'staged'
                CHECK (rung IN ('staged','sampled','trusted')),
    streak      INTEGER NOT NULL DEFAULT 0,
    promoted_at TEXT,
    demoted_at  TEXT,
    updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (proposer, predicate)
) WITHOUT ROWID;
"#;

const V011_OBSERVATION_CONFIDENCE: &str = r#"
-- Per-sighting declared confidence (Option D, PLAN.md settled
-- 2026-08-12): the founding 'asserted' row's value is the immutable
-- prior anchor for classes with no review history (deterministic
-- extractors — alias, attendee — never pass through review, and their
-- declared confidence is the only calibration signal they have).
-- Backfill approximates the founding declaration with the current
-- stored value (post-ratchet MAX; close enough as an anchor).
ALTER TABLE fact_observation ADD COLUMN confidence REAL;
UPDATE fact_observation SET confidence =
    (SELECT f.confidence FROM fact f WHERE f.id = fact_observation.fact_id)
WHERE kind = 'asserted';
"#;

const V010_FACT_OBSERVATION: &str = r#"
-- fact_observation (PLAN.md Wave 2a): how-known AND how-verified,
-- first-class. Generalizes the single-episode acquisition provenance
-- (fact.episode_id + observation_count) to one row per sighting. This
-- one table is the corroboration counter, goal-2's staleness clock,
-- the sensitivity MAX-over-contributors, the evidence-rooted-support
-- guard, and the verification audit trail.
CREATE TABLE IF NOT EXISTS fact_observation (
    id          INTEGER PRIMARY KEY,
    fact_id     INTEGER NOT NULL REFERENCES fact(id) ON DELETE CASCADE,
    -- SET NULL, not CASCADE: redacting an episode (§10) keeps the sighting
    -- as an identifier-only record (tombstone doctrine) so corroboration
    -- arithmetic on OTHER facts it touched stays honest.
    episode_id  INTEGER REFERENCES episode(id) ON DELETE SET NULL,
    observed_at TEXT NOT NULL DEFAULT (datetime('now')),
    kind        TEXT NOT NULL CHECK (kind IN
                  ('asserted','corroborated','verified','disputed','corrected')),
    method      TEXT NOT NULL   -- extractor name | verifier-deref | gossip:tier1/2 | research:web | user
);
CREATE INDEX IF NOT EXISTS idx_fact_observation_fact
    ON fact_observation(fact_id, observed_at);

-- Backfill: every fact gets its founding 'asserted' row from the
-- acquisition provenance it already stores. Prior corroborations
-- (observation_count > 1) cannot be reconstructed into distinct rows —
-- the counter carries forward as-is; new sightings add rows from now on.
INSERT INTO fact_observation (fact_id, episode_id, observed_at, kind, method)
SELECT id, episode_id, ingested_at, 'asserted', COALESCE(extractor, 'unknown')
FROM fact;
"#;

const V009_LEDGER: &str = r#"
-- The measurement substrate (PLAN.md Wave 1b). Three tables:
--
-- query_log — every routed query with its coverage verdict. Serves three
-- masters: deferred research (status='gap' is the nightly work queue),
-- gold-set mining (real misses, not re-mined corpus), and speed (recurring
-- shapes worth materializing).
CREATE TABLE IF NOT EXISTS query_log (
    id            INTEGER PRIMARY KEY,
    ts            TEXT NOT NULL DEFAULT (datetime('now')),
    tool          TEXT NOT NULL,      -- cli.query | tui.search | mcp.kg_search
    query         TEXT NOT NULL,
    intent        TEXT,               -- lookup | recall | aggregate
    anchor_ids    TEXT,               -- JSON array of resolved node ids
    top_score     REAL,
    result_count  INTEGER NOT NULL,
    coverage_flags TEXT,              -- JSON array: empty | thin | ambiguous
    status        TEXT NOT NULL DEFAULT 'ok'  -- ok | gap | researched | resolved
);
CREATE INDEX IF NOT EXISTS idx_query_log_status ON query_log(status);
CREATE INDEX IF NOT EXISTS idx_query_log_ts ON query_log(ts);

-- retrieval_touch — ACT-R base-level demand (§11.5): bumped for every item
-- that enters a returned pack and every resolved anchor. touches + first_at
-- feed the optimized-learning activation approximation; last_at feeds decay.
CREATE TABLE IF NOT EXISTS retrieval_touch (
    kind     TEXT NOT NULL,           -- node | fact | episode
    ref_id   TEXT NOT NULL,           -- node id or uid
    touches  INTEGER NOT NULL DEFAULT 1,
    first_at TEXT NOT NULL,
    last_at  TEXT NOT NULL,
    PRIMARY KEY (kind, ref_id)
) WITHOUT ROWID;

-- event_log — append-only observability spine (PLAN.md): flag_shown,
-- flag_actioned, question_asked/answered, correction, promotion, demotion,
-- sweep, probe_run. Metrics are SQL views over this + query_log.
CREATE TABLE IF NOT EXISTS event_log (
    id      INTEGER PRIMARY KEY,
    ts      TEXT NOT NULL DEFAULT (datetime('now')),
    kind    TEXT NOT NULL,
    ref     TEXT,
    payload TEXT
);
CREATE INDEX IF NOT EXISTS idx_event_log_kind ON event_log(kind, ts);
"#;

const V008_FACT_SENSITIVITY: &str = r#"
-- §10: derived facts inherit their evidence's sensitivity — extraction is a
-- hop, and hops don't launder. Backfill copies the source episode's tier
-- (single episode_id today; fact_observation will generalize to MAX over all
-- contributors). Facts with no episode (manual/test) default to 'personal'.
ALTER TABLE fact ADD COLUMN sensitivity TEXT NOT NULL DEFAULT 'personal';
UPDATE fact SET sensitivity = COALESCE(
    (SELECT e.sensitivity FROM episode e WHERE e.id = fact.episode_id),
    'personal');
"#;

const V007_EPISODE_TOMBSTONE: &str = r#"
-- Deletion memory: redacting an episode records its (source, source_id) so
-- re-ingest can't resurrect it — whole-file sources (ICS, reflect zips, mbox)
-- re-present every item on every sync. Identifiers only, no content, so a
-- §10 true delete stays true. Cleared by undo or `pkg tombstone rm`.
CREATE TABLE IF NOT EXISTS episode_tombstone (
    source TEXT NOT NULL,
    source_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (source, source_id)
) WITHOUT ROWID;
"#;

const V006_UNDO_LOG: &str = r#"
-- Undo snapshots for TUI episode deletes/edits. The privacy path
-- (`pkg redact`) bypasses this deliberately — an undo copy would defeat a
-- §10 true delete. Rows are consumed by `pkg undo` (newest first).
CREATE TABLE IF NOT EXISTS undo_log (
    id INTEGER PRIMARY KEY,
    action TEXT NOT NULL CHECK (action IN ('delete','edit')),
    ref_uid TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    snapshot TEXT NOT NULL
);
"#;

pub fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )?;

    for migration in MIGRATIONS {
        let already_applied: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM _migrations WHERE version = ?1",
            [migration.version],
            |row| row.get(0),
        )?;

        if !already_applied {
            // Transaction so a partial migration failure doesn't corrupt the DB.
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(migration.sql)?;
            tx.execute(
                "INSERT INTO _migrations (version, name) VALUES (?1, ?2)",
                rusqlite::params![migration.version, migration.name],
            )?;
            tx.commit()?;
        }
    }

    Ok(())
}

const V002_EXTRACT_STATE: &str = r#"
-- Tracks which episodes have been through Tier-7 LLM extraction (§7), so
-- re-runs only touch new material. schema_version mirrors the envelope's:
-- bump it to re-extract everything after a prompt improvement.
CREATE TABLE IF NOT EXISTS extract_state (
    episode_id INTEGER PRIMARY KEY REFERENCES episode(id) ON DELETE CASCADE,
    model TEXT,
    prompt_version INTEGER NOT NULL DEFAULT 1,
    candidates_created INTEGER NOT NULL DEFAULT 0,
    extracted_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

const V003_EPISODE_RAW: &str = r#"
-- Raw-capture archive (retention policy 'capture'/'capture_delete'): the
-- full source content of an episode, stored INSIDE the encrypted DB so the
-- plaintext file can be deleted after ingest. Not FTS-indexed — retrieval
-- works on the distilled body; this is the provenance/re-enrichment archive.
CREATE TABLE IF NOT EXISTS episode_raw (
    episode_id  INTEGER PRIMARY KEY REFERENCES episode(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    captured_at TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

// Recreate the FTS indexes with porter stemming so "discussions" matches
// "discussion" and "grades" matches "grade" — the memory job asks in
// paraphrase, not in the corpus's exact inflections. External-content
// tables carry no data of their own; 'rebuild' re-indexes from the host
// tables. The sync triggers reference the tables by name and survive.
const V005_FTS_PORTER: &str = r#"
DROP TABLE IF EXISTS fts_episode;
DROP TABLE IF EXISTS fts_fact;
CREATE VIRTUAL TABLE fts_episode USING fts5(body, content='episode', content_rowid='id', tokenize='porter unicode61');
CREATE VIRTUAL TABLE fts_fact    USING fts5(statement, content='fact', content_rowid='id', tokenize='porter unicode61');
INSERT INTO fts_episode(fts_episode) VALUES('rebuild');
INSERT INTO fts_fact(fts_fact) VALUES('rebuild');
"#;

const V004_EPISODE_ANNOTATION: &str = r#"
-- Human annotations on episodes: tags and free-text notes attached during
-- search/review. Curation metadata, not content — kept out of FTS (the
-- distilled body stays the retrieval surface); tags are queried directly.
-- Episodes are rows, not nodes, so node_field can't hold these.
CREATE TABLE IF NOT EXISTS episode_annotation (
    id INTEGER PRIMARY KEY,
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('tag','note')),
    body TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(episode_id, kind, body)
);
CREATE INDEX IF NOT EXISTS idx_annotation_episode ON episode_annotation(episode_id);
CREATE INDEX IF NOT EXISTS idx_annotation_kind_body ON episode_annotation(kind, body);
"#;

const V001_INITIAL_SCHEMA: &str = r#"
-- ============================================================
-- Entity layer (§4.2) — nodes carry scope_id (not card_id);
-- aliases live in an indexed table, not a JSON column.
-- ============================================================

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    node_type TEXT NOT NULL,            -- closed set, enforced in code (§4.2)
    name TEXT NOT NULL,
    canonical_name TEXT NOT NULL,
    properties TEXT NOT NULL DEFAULT '{}',
    confidence REAL NOT NULL DEFAULT 1.0,
    source TEXT NOT NULL DEFAULT 'manual',
    source_ref TEXT,
    scope_id TEXT,                      -- single primary parent: the inheritance chain (§4.5)
    access_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(node_type);
CREATE INDEX IF NOT EXISTS idx_nodes_canonical ON nodes(canonical_name);
CREATE INDEX IF NOT EXISTS idx_nodes_scope ON nodes(scope_id);
CREATE INDEX IF NOT EXISTS idx_nodes_updated ON nodes(updated_at);

CREATE TABLE IF NOT EXISTS node_alias (
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    alias   TEXT NOT NULL,              -- stored lowercased
    source  TEXT,                       -- attendee|manual|llm|merge
    PRIMARY KEY (node_id, alias)
);
CREATE INDEX IF NOT EXISTS idx_alias_lookup ON node_alias(alias);

-- One table for every deterministic identity key (generalizes node_emails).
CREATE TABLE IF NOT EXISTS node_identifier (
    kind       TEXT NOT NULL,           -- email|phone|slack_uid|handle|orcid|url|doi|path
    value      TEXT NOT NULL,           -- normalized: lowercase email, E.164 phone, canonical URL
    node_id    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    source     TEXT,
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (kind, value)
);
CREATE INDEX IF NOT EXISTS idx_identifier_node ON node_identifier(node_id);

-- ============================================================
-- Provenance layer (§4.1) — immutable source records, append-only.
-- INTEGER pk is load-bearing: vec0 + FTS5 external content key on rowid.
-- ============================================================

CREATE TABLE IF NOT EXISTS episode (
    id            INTEGER PRIMARY KEY,
    uid           TEXT NOT NULL UNIQUE,
    source        TEXT NOT NULL,        -- bee.conversation|bee.daily|email.thread|slack.thread|
                                        -- calendar.event|session.hermes|session.claude|note|sms
    source_id     TEXT NOT NULL,        -- provider id — makes re-ingest idempotent
    source_ref    TEXT,                 -- pointer back to raw (file path, session id)
    body          TEXT NOT NULL,
    occurred_at   TEXT NOT NULL,        -- VALID time
    occurred_end  TEXT,                 -- intervals matter for the calendar×Bee join
    ingested_at   TEXT NOT NULL DEFAULT (datetime('now')),
    content_hash  TEXT NOT NULL,
    lat REAL, lon REAL, location TEXT,
    sensitivity   TEXT NOT NULL DEFAULT 'personal',   -- public|personal|private|secret (§10)
    scope_id      TEXT,
    meta          TEXT,                 -- JSON
    UNIQUE (source, source_id)
);
CREATE INDEX IF NOT EXISTS idx_episode_time ON episode(occurred_at);
CREATE INDEX IF NOT EXISTS idx_episode_source ON episode(source);

-- The M:N substrate: co-occurrence, salience, person-filtered search (§4.1).
CREATE TABLE IF NOT EXISTS mention (
    episode_id INTEGER NOT NULL REFERENCES episode(id) ON DELETE CASCADE,
    node_id    TEXT    NOT NULL REFERENCES nodes(id)   ON DELETE CASCADE,
    extractor  TEXT NOT NULL,           -- regex|alias|attendee|llm|manual|temporal_join
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (episode_id, node_id)
);
CREATE INDEX IF NOT EXISTS idx_mention_node ON mention(node_id);

-- ============================================================
-- Fact layer (§4.3) — one table, bi-temporal.
-- ============================================================

CREATE TABLE IF NOT EXISTS predicate (
    name TEXT PRIMARY KEY,
    inverse TEXT,
    description TEXT
);
CREATE TABLE IF NOT EXISTS predicate_alias (
    alias TEXT PRIMARY KEY,
    name TEXT REFERENCES predicate(name)
);

CREATE TABLE IF NOT EXISTS fact (
    id            INTEGER PRIMARY KEY,
    uid           TEXT NOT NULL UNIQUE,
    subject_id    TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    predicate     TEXT NOT NULL REFERENCES predicate(name),
    object_id     TEXT REFERENCES nodes(id),   -- NULL ⇒ attribute-style fact
    object_value  TEXT,                        -- literal, when the object isn't a node
    statement     TEXT NOT NULL,               -- NL sentence — the embed/BM25 target
    episode_id    INTEGER REFERENCES episode(id),
    valid_from    TEXT,                        -- valid time (true in the world)
    valid_to      TEXT,                        -- NULL = still true
    ingested_at   TEXT NOT NULL DEFAULT (datetime('now')),   -- system time
    invalidated_at TEXT,                       -- NULL = still believed
    confidence    REAL NOT NULL DEFAULT 0.7,
    weight        REAL NOT NULL DEFAULT 1.0,
    observation_count INTEGER NOT NULL DEFAULT 1,
    extractor     TEXT,                        -- alias|attendee|npmi|temporal_join|llm|manual
    tags          TEXT
);
CREATE INDEX IF NOT EXISTS idx_fact_subject ON fact(subject_id);
CREATE INDEX IF NOT EXISTS idx_fact_object  ON fact(object_id);
CREATE INDEX IF NOT EXISTS idx_fact_pred    ON fact(predicate);
CREATE INDEX IF NOT EXISTS idx_fact_episode ON fact(episode_id);

-- One LIVE fact per (subject, predicate, object); unlimited history behind it.
CREATE UNIQUE INDEX IF NOT EXISTS idx_fact_live ON fact(subject_id, predicate, object_id)
    WHERE valid_to IS NULL AND invalidated_at IS NULL;

CREATE VIEW IF NOT EXISTS fact_current AS
    SELECT * FROM fact WHERE valid_to IS NULL AND invalidated_at IS NULL;

-- `edges` is a VIEW: the subset of facts with both endpoints resolved.
CREATE VIEW IF NOT EXISTS edges AS
    SELECT uid AS id, subject_id AS from_id, predicate, object_id AS to_id, weight, tags
    FROM fact_current WHERE object_id IS NOT NULL;

-- Staging before promotion: the sole write path for anything non-deterministic.
CREATE TABLE IF NOT EXISTS fact_candidate (
    id INTEGER PRIMARY KEY,
    payload TEXT NOT NULL,              -- proposed fact, same shape (JSON)
    status TEXT NOT NULL DEFAULT 'proposed',   -- proposed|accepted|rejected
    proposed_by TEXT,
    episode_id INTEGER,
    confidence REAL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    reviewed_at TEXT,
    reject_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_candidate_status ON fact_candidate(status);

-- ============================================================
-- Productivity layer (§4.4) — Goal / Area / Project / Task.
-- Each is a node plus a detail table for indexed mutable state.
-- ============================================================

CREATE TABLE IF NOT EXISTS task_detail (
    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'inbox',      -- inbox|next|waiting|scheduled|done|dropped
    task_type TEXT NOT NULL DEFAULT 'action',  -- action|compose|research|waiting (GTD)
    due_at TEXT,
    defer_until TEXT,
    estimate_min INTEGER,
    context_tag TEXT,
    priority_score REAL DEFAULT 0.0,
    priority_factors TEXT DEFAULT '{}',
    completed_at TEXT,
    parent_id TEXT REFERENCES nodes(id),
    scope_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_task_status ON task_detail(status);

CREATE TABLE IF NOT EXISTS project_detail (
    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'active',     -- active|paused|done|dropped
    outcome TEXT DEFAULT '',
    target_date TEXT,
    completed_at TEXT,
    last_activity_at TEXT,                     -- stall detection
    review_interval_days INTEGER DEFAULT 7
);

CREATE TABLE IF NOT EXISTS goal_detail (
    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    status TEXT NOT NULL DEFAULT 'active',     -- active|achieved|abandoned
    horizon TEXT,
    target_date TEXT,
    success_criteria TEXT DEFAULT '',
    progress REAL DEFAULT 0.0,
    last_reviewed_at TEXT,
    review_interval_days INTEGER DEFAULT 30
);

-- ============================================================
-- Context layer (§4.5) — instructions, not cards.
-- ============================================================

CREATE TABLE IF NOT EXISTS node_context (
    node_id            TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    instruction        TEXT DEFAULT '',   -- hand-authored. NEVER auto-modified.
    summary            TEXT DEFAULT '',   -- generated, refreshable (materialized view)
    summary_updated_at TEXT,
    summary_stale      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS assign_rule (
    id       TEXT PRIMARY KEY,
    node_id  TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    rule     TEXT NOT NULL,               -- JSON predicate (§7 tier 1b)
    enabled  INTEGER NOT NULL DEFAULT 1,
    match_count INTEGER NOT NULL DEFAULT 0,
    last_matched_at TEXT
);

CREATE TABLE IF NOT EXISTS node_field (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    label TEXT,
    field_type TEXT,
    options TEXT,
    is_required INTEGER,
    position INTEGER
);

-- ============================================================
-- Enrichment envelope (§6) — one shape for every source.
-- ============================================================

CREATE TABLE IF NOT EXISTS episode_enrichment (
    episode_id INTEGER PRIMARY KEY REFERENCES episode(id) ON DELETE CASCADE,
    schema_version INTEGER NOT NULL DEFAULT 1,
    payload TEXT NOT NULL,
    model TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- ============================================================
-- Retrieval + rollup tables (§4.6).
-- ============================================================

CREATE VIRTUAL TABLE IF NOT EXISTS fts_episode USING fts5(body, content='episode', content_rowid='id');
CREATE VIRTUAL TABLE IF NOT EXISTS fts_fact    USING fts5(statement, content='fact', content_rowid='id');

-- Keep FTS5 external-content indexes in sync with their host tables.
CREATE TRIGGER IF NOT EXISTS trg_episode_ai AFTER INSERT ON episode BEGIN
    INSERT INTO fts_episode(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS trg_episode_ad AFTER DELETE ON episode BEGIN
    INSERT INTO fts_episode(fts_episode, rowid, body) VALUES ('delete', old.id, old.body);
END;
CREATE TRIGGER IF NOT EXISTS trg_episode_au AFTER UPDATE OF body ON episode BEGIN
    INSERT INTO fts_episode(fts_episode, rowid, body) VALUES ('delete', old.id, old.body);
    INSERT INTO fts_episode(rowid, body) VALUES (new.id, new.body);
END;
CREATE TRIGGER IF NOT EXISTS trg_fact_ai AFTER INSERT ON fact BEGIN
    INSERT INTO fts_fact(rowid, statement) VALUES (new.id, new.statement);
END;
CREATE TRIGGER IF NOT EXISTS trg_fact_ad AFTER DELETE ON fact BEGIN
    INSERT INTO fts_fact(fts_fact, rowid, statement) VALUES ('delete', old.id, old.statement);
END;
CREATE TRIGGER IF NOT EXISTS trg_fact_au AFTER UPDATE OF statement ON fact BEGIN
    INSERT INTO fts_fact(fts_fact, rowid, statement) VALUES ('delete', old.id, old.statement);
    INSERT INTO fts_fact(rowid, statement) VALUES (new.id, new.statement);
END;

CREATE VIRTUAL TABLE IF NOT EXISTS vec_episode USING vec0(episode_id INTEGER PRIMARY KEY, embedding FLOAT[768]);
CREATE VIRTUAL TABLE IF NOT EXISTS vec_fact    USING vec0(fact_id    INTEGER PRIMARY KEY, embedding FLOAT[768]);

-- Makes "when did I last meet June?" a primary-key lookup, not a scan.
CREATE TABLE IF NOT EXISTS person_interaction (
    node_id TEXT PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    first_seen_at TEXT,
    last_seen_at TEXT,
    last_channel TEXT,
    last_episode_id TEXT,
    interaction_count INTEGER NOT NULL DEFAULT 0,
    last_meeting_at TEXT,   -- calendar attendance   ← "met"
    last_spoken_at  TEXT,   -- Bee co-presence       ← "met"
    last_email_at   TEXT,
    last_message_at TEXT,
    last_slack_at   TEXT
);

-- Cross-system task mirroring (Bee todos / FlowMail tasks / Hermes kanban).
CREATE TABLE IF NOT EXISTS external_ref (
    node_id TEXT NOT NULL REFERENCES nodes(id) ON DELETE CASCADE,
    system TEXT NOT NULL,
    external_id TEXT NOT NULL,
    last_synced TEXT,
    sync_mode TEXT NOT NULL DEFAULT 'pull',
    PRIMARY KEY (system, external_id)
);

CREATE TABLE IF NOT EXISTS ingest_state (
    source TEXT PRIMARY KEY,
    cursor TEXT,
    last_run_at TEXT,
    last_ok_at TEXT,
    items_seen INTEGER DEFAULT 0,
    last_error TEXT
);

-- ============================================================
-- Controlled predicate vocabulary seed (§4.3, §4.4).
-- ============================================================

INSERT OR IGNORE INTO predicate (name, inverse, description) VALUES
    ('pursued_via',      'pursues',          'Goal is pursued via Project'),
    ('realized_by',      'realizes',         'Project is realized by Task'),
    ('contains',         'contained_in',     'Area contains Project or Task'),
    ('blocked_by',       'blocks',           'Task is blocked by Task'),
    ('waiting_on',       'owes',             'Task is waiting on Person'),
    ('assigned_to',      'assigned',         'Task is assigned to Person'),
    ('originated_in',    'originated',       'Task/fact originated in Episode'),
    ('discussed_at',     'discussion_of',    'Topic/task discussed at Event'),
    ('discussed_during', 'discussion_of',    'Conversation overlapped Event (temporal join)'),
    ('attended',         'attended_by',      'Person attended Event'),
    ('organized',        'organized_by',     'Person organized Event'),
    ('about',            'subject_of',       'Item is about Topic/Project'),
    ('works_at',         'employs',          'Person works at Org'),
    ('works_on',         'worked_on_by',     'Person works on Project'),
    ('collaborates_with','collaborates_with','Person collaborates with Person'),
    ('member_of',        'has_member',       'Person is member of Org/Area'),
    ('located_in',       'location_of',      'Entity is located in Place'),
    ('mentions',         'mentioned_in',     'Episode mentions Node'),
    ('related_to',       'related_to',       'Generic association'),
    ('authored',         'authored_by',      'Person authored Artifact/Document'),
    ('uses',             'used_by',          'Project uses Artifact/Tool');

INSERT OR IGNORE INTO predicate_alias (alias, name) VALUES
    ('working_on', 'works_on'),
    ('is_working_on', 'works_on'),
    ('work_on', 'works_on'),
    ('employed_by', 'works_at'),
    ('works_for', 'works_at'),
    ('collaborates', 'collaborates_with'),
    ('collab_with', 'collaborates_with'),
    ('relates_to', 'related_to'),
    ('regarding', 'about'),
    ('waiting_for', 'waiting_on'),
    ('blocked_on', 'blocked_by'),
    ('wrote', 'authored');
"#;

/// Which model produced the vectors currently in the store.
///
/// Added 2026-08-20 with the move off nomic-embed-text. Nothing about a vector
/// reveals what made it: a 768-dim nomic vector and a 768-dim truncated Qwen
/// vector are indistinguishable, and a store holding both answers queries
/// confidently and wrongly. `precheck`'s duplicate thresholds encode one
/// model's cosine scale, so a silent swap moves what 0.93 means.
///
/// It is a migration rather than a CREATE TABLE IF NOT EXISTS inside the
/// writer, which is where it started: a table created lazily by one function is
/// invisible to everything that reasons about the schema. `db::copy_all_tables`
/// walks a fixed list, so the lazily-created version was silently dropped from
/// every `decrypt` snapshot — found by checking a snapshot rather than by any
/// failure.
const V016_EMBED_META: &str = r#"
CREATE TABLE IF NOT EXISTS embed_meta (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    model       TEXT NOT NULL,
    dims        INTEGER NOT NULL,
    instruction TEXT NOT NULL,
    written_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// What the co-occurrence input-set alarm has already reported.
///
/// The alarm fires when a co-occurrence belief's *stated* episode count —
/// frozen in its statement prose at mint time — exceeds a live recount by
/// more than [`crate::decay::COLLAPSE_RATIO`], and it tells the operator to
/// "check the mention pipeline". It had no memory, so it re-reported the
/// identical set every night: 54 alarms a night from 2026-08-25 to
/// 2026-09-01, and 44-48 a night before that, with no night's output
/// distinguishable from the last.
///
/// They were not a pipeline fault. Every step change in the count lands on a
/// day an operator re-partitioned entities — 0 → 48 on 2026-08-14
/// (a person merge), 48 → 193 → 46 on 2026-08-15 (a phantom repair),
/// 44 → 55 on 2026-08-25 (a relink and a person split). A merge moves
/// episodes between nodes by design, so the stated number is right as of
/// mint and the recount is right as of now, and neither says a mention was
/// lost. `decay.rs`'s own comment records that the 2026-08-15 investigation
/// found zero beliefs without support; the same shape survived it.
///
/// There is no merge audit table to consult, so this does not try to
/// classify the cause. It records that the alarm was raised and at what
/// count, which is enough to report NEW and WORSENED collapses loudly and
/// keep the unchanged ones as a number — an alarm that repeats identically
/// for eighteen days informs nobody, and trains a reader to skip the line
/// where a real one would appear.
const V024_COOCCURRENCE_ALARM: &str = r#"
CREATE TABLE IF NOT EXISTS cooccurrence_alarm (
    fact_uid      TEXT PRIMARY KEY,
    stated_co     INTEGER NOT NULL,
    observed_co   INTEGER NOT NULL,
    first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
    last_seen_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
"#;

/// The observation at a collapse's FIRST sighting, never overwritten.
///
/// `observed_co` is rewritten on every non-dry sweep, so the
/// "(was N when first reported)" message named the *previous* night — a pair
/// eroding 50 → 40 → 30 reported "was 40" on the third. `COALESCE` in the
/// reader keeps rows written before this migration meaningful: they degrade
/// to their latest observation, which is the best that row can honestly say.
///
/// **A separate migration rather than an edit to V024, because V024 has
/// already run.** Editing a migration in place works only on a database that
/// has never applied it: `run_migrations` skips a version already recorded,
/// so a store that took V024 last night would never gain the column and
/// every read of it fails with `no such column`. Found exactly that way —
/// the in-memory tests all build from a fresh `run_migrations` and passed,
/// and a sweep against a copy of the live store did not.
const V025_COOCCURRENCE_ALARM_FIRST_OBSERVED: &str = r#"
ALTER TABLE cooccurrence_alarm ADD COLUMN first_observed_co INTEGER;
"#;
