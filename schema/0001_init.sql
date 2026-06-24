-- wicked-agent — session-ledger DDL (ARCHITECTURE §4). Phase-1 skeleton: DDL only.
--
-- Local SQLite, dual-write JSON-first per the collection convention. Everything
-- here is a PROJECTION with pointers back to the authoritative sibling
-- (orchestration = events, council = rankings, governance = conformance,
-- vault = evidence). Nothing here is the source of truth; if the ledger is
-- lost, the authoritative record still lives in the siblings.
--
-- Idempotent: CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS so the
-- migration is safe to re-run. Schema version is tracked in `meta`.

-- sessions: one row per `wicked-agent run`; workflow correlation lives in orchestration.
CREATE TABLE IF NOT EXISTS sessions (
  session_id        TEXT PRIMARY KEY,
  workflow_id       TEXT,            -- = orchestration correlationId
  repos             TEXT,
  problem           TEXT,
  entity_mode       TEXT,            -- 'shared' | 'isolated'  (ADR-0001)
  collection_scope  TEXT,            -- shared scope id, or per-CLI when isolated
  vault_scope       TEXT,
  status            TEXT,            -- planning|distributing|executing|completed|halted
  created_at        TEXT,
  updated_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_sess_status ON sessions(status);

-- wrapped_clis: which CLIs this session wrapped + council's capability matrix.
CREATE TABLE IF NOT EXISTS wrapped_clis (
  session_id        TEXT,
  cli_id            TEXT,            -- e.g. 'claude','gemini'
  probe_ref         TEXT,            -- pointer to council's probe result (authoritative)
  supports_skills   INTEGER,         -- capability matrix
  supports_hook     INTEGER,
  supports_env      INTEGER,
  governance_mode   TEXT,            -- 'pretool-hook' | 'post-hoc'  (ADR-0003)
  scope             TEXT,            -- collection scope this CLI writes to
  PRIMARY KEY (session_id, cli_id)
);

-- work_units: the plan's units; each maps to a phase + a council verdict + vault evidence.
CREATE TABLE IF NOT EXISTS work_units (
  unit_id           TEXT PRIMARY KEY,
  session_id        TEXT,
  ord               INTEGER,         -- topological order within the session
  description       TEXT,
  assigned_cli      TEXT,            -- chosen via wicked.council.voted (council-owned)
  council_verdict_ref TEXT,          -- pointer to the verdict
  phase_ref         TEXT,            -- pointer to the orchestration phase
  conformance_ref   TEXT,            -- pointer to governance ConformanceClaim, if gated
  evidence_id       TEXT,            -- pointer into wicked-vault
  status            TEXT,            -- pending|running|gated|done|rejected
  updated_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_unit_session ON work_units(session_id, ord);

-- meta: schema_version, last_event_seen (bus cursor).
CREATE TABLE IF NOT EXISTS meta (
  k TEXT PRIMARY KEY,
  v TEXT
);
