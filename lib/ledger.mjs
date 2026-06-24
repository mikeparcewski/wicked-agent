// wicked-agent — the session ledger (ARCHITECTURE §4: OWNED, but a projection).
//
// JSON-first under ./.wicked-agent/ (no native deps — the schema/0001_init.sql
// SQLite shape is a later optimization; this is its json-only degrade twin, exactly
// like governance Batch A and orchestration Batch B). Everything here is a PROJECTION
// with *_ref pointers back to the authoritative sibling (orchestration phases,
// council verdicts, governance conformance, vault evidence). Lose the ledger and the
// authoritative record still lives in the siblings.
//
// Layout (under dataDir, default ./.wicked-agent/):
//   sessions/<sessionId>.json -> { session, wrapped_clis: [...], work_units: [...] }

import { mkdirSync, readFileSync, writeFileSync, existsSync, readdirSync } from "node:fs";
import { join, isAbsolute } from "node:path";

export const DEFAULT_DATA_DIR = ".wicked-agent";

function resolveDataDir(dataDir = DEFAULT_DATA_DIR) {
  return isAbsolute(dataDir) ? dataDir : join(process.cwd(), dataDir);
}
function sessionsDir(dataDir) {
  return join(resolveDataDir(dataDir), "sessions");
}
function sessionFile(dataDir, sessionId) {
  return join(sessionsDir(dataDir), `${encodeURIComponent(sessionId)}.json`);
}
function nowIso() {
  return new Date().toISOString();
}

function readDoc(dataDir, sessionId) {
  const path = sessionFile(dataDir, sessionId);
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return null;
  }
}
function writeDoc(dataDir, sessionId, doc) {
  const dir = sessionsDir(dataDir);
  mkdirSync(dir, { recursive: true });
  writeFileSync(sessionFile(dataDir, sessionId), JSON.stringify(doc, null, 2) + "\n", "utf8");
}

export const ledger = {
  /**
   * Create (or idempotently return) a session row.
   * @param {object} session  { session_id, workflow_id, repos, problem, entity_mode,
   *                            collection_scope, vault_scope, status }
   * @param {{ dataDir?: string }} [opts]
   */
  createSession(session, opts = {}) {
    if (!session?.session_id) throw new Error("createSession requires session_id");
    const existing = readDoc(opts.dataDir, session.session_id);
    if (existing) return existing;
    const at = nowIso();
    const doc = {
      session: { ...session, created_at: at, updated_at: at },
      wrapped_clis: [],
      work_units: [],
    };
    writeDoc(opts.dataDir, session.session_id, doc);
    return doc;
  },

  /** Merge fields onto the session row (e.g. status transitions). */
  updateSession(sessionId, patch, opts = {}) {
    const doc = readDoc(opts.dataDir, sessionId);
    if (!doc) throw new Error(`updateSession: no such session '${sessionId}'`);
    doc.session = { ...doc.session, ...patch, updated_at: nowIso() };
    writeDoc(opts.dataDir, sessionId, doc);
    return doc.session;
  },

  /** Record a wrapped CLI + its capability/scope row. */
  recordWrappedCli(sessionId, cli, opts = {}) {
    const doc = readDoc(opts.dataDir, sessionId);
    if (!doc) throw new Error(`recordWrappedCli: no such session '${sessionId}'`);
    doc.wrapped_clis = (doc.wrapped_clis || []).filter((c) => c.cli_id !== cli.cli_id);
    doc.wrapped_clis.push(cli);
    writeDoc(opts.dataDir, sessionId, doc);
    return cli;
  },

  /** Upsert a work-unit row (by unit_id). */
  upsertWorkUnit(sessionId, unit, opts = {}) {
    const doc = readDoc(opts.dataDir, sessionId);
    if (!doc) throw new Error(`upsertWorkUnit: no such session '${sessionId}'`);
    const idx = (doc.work_units || []).findIndex((u) => u.unit_id === unit.unit_id);
    const row = { ...unit, updated_at: nowIso() };
    if (idx >= 0) doc.work_units[idx] = { ...doc.work_units[idx], ...row };
    else doc.work_units.push(row);
    writeDoc(opts.dataDir, sessionId, doc);
    return row;
  },

  /** Read the whole session ledger doc (or null). */
  getSession(sessionId, opts = {}) {
    return readDoc(opts.dataDir, sessionId);
  },

  /** List all session ids on disk (sorted). */
  listSessions(opts = {}) {
    const dir = sessionsDir(opts.dataDir);
    if (!existsSync(dir)) return [];
    return readdirSync(dir)
      .filter((n) => n.endsWith(".json"))
      .map((n) => decodeURIComponent(n.replace(/\.json$/, "")))
      .sort();
  },
};

export default ledger;
