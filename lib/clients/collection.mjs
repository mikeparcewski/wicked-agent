// wicked-agent — collection client seam (ARCHITECTURE §8, §6; ADR-0001).
//
// Reuse intent: the collection (wicked-memory / wicked-knowledge / wicked-overlay)
// is the shared memory/knowledge/code-graph that makes many CLIs ONE entity. Full
// wicked-memory/overlay integration is a NOTED follow-up (ARCHITECTURE §6, ADR-0001
// cold-start note); Batch D lands a JSON-first shared store keyed by collection
// scope so the headline claim is exercised end-to-end NOW: two CLIs pinned to the
// SAME scope read each other's artifacts; under `isolated` they cannot.
//
// "Don't build a third (here, fourth) storage layer" — this is a thin scope-keyed
// JSON projection, the json-only degrade shape every sibling already uses, with the
// wicked-memory adapter as the documented upgrade path. The shared-vs-isolated
// TOGGLE is owned here via lib/scope.mjs; the isolation MECHANISM is the reused
// concept (per-scope keyspace), not a re-invented store.
//
// Layout (under dataDir, default ./.wicked-agent/):
//   collection/<urlencoded scope>.json   -> { scope, entries: { <key>: artifact } }

import { mkdirSync, readFileSync, writeFileSync, existsSync } from "node:fs";
import { join, isAbsolute } from "node:path";

import { resolveScope } from "../scope.mjs";

export const DEFAULT_DATA_DIR = ".wicked-agent";

function resolveDataDir(dataDir = DEFAULT_DATA_DIR) {
  return isAbsolute(dataDir) ? dataDir : join(process.cwd(), dataDir);
}

function collectionDir(dataDir) {
  return join(resolveDataDir(dataDir), "collection");
}

function scopeFile(dataDir, scope) {
  return join(collectionDir(dataDir), `${encodeURIComponent(scope)}.json`);
}

function readScopeDoc(dataDir, scope) {
  const path = scopeFile(dataDir, scope);
  if (!existsSync(path)) return { scope, entries: {} };
  try {
    return JSON.parse(readFileSync(path, "utf8"));
  } catch {
    return { scope, entries: {} };
  }
}

function writeScopeDoc(dataDir, scope, doc) {
  const dir = collectionDir(dataDir);
  mkdirSync(dir, { recursive: true });
  writeFileSync(scopeFile(dataDir, scope), JSON.stringify(doc, null, 2) + "\n", "utf8");
}

export const collection = {
  /**
   * Resolve the collection scope a CLI writes to (shared vs isolated, §6).
   * Thin pass-through to lib/scope.mjs so callers have one entry point.
   * @param {{ entityMode: "shared"|"isolated", sessionId: string, cliId: string }} args
   */
  scopeFor(args) {
    return resolveScope(args);
  },

  /**
   * Write an artifact into a collection scope under a key.
   * @param {string} scope     a resolved scope id (from scopeFor / resolveScope).
   * @param {string} key       the artifact key (e.g. a unit id).
   * @param {*}      artifact   the artifact payload (JSON-serializable).
   * @param {{ dataDir?: string }} [opts]
   * @returns {{ scope: string, key: string, path: string }}
   */
  write(scope, key, artifact, opts = {}) {
    if (!scope || typeof scope !== "string") throw new Error("collection.write requires a scope");
    if (!key || typeof key !== "string") throw new Error("collection.write requires a key");
    const dataDir = opts.dataDir;
    const doc = readScopeDoc(dataDir, scope);
    doc.scope = scope;
    if (!doc.entries || typeof doc.entries !== "object") doc.entries = {};
    doc.entries[key] = { artifact, written_at: new Date().toISOString() };
    writeScopeDoc(dataDir, scope, doc);
    return { scope, key, path: scopeFile(dataDir, scope) };
  },

  /**
   * Read an artifact from a collection scope. With a key -> that entry's artifact
   * (or null). Without a key -> the whole entries map (what THIS scope can recall).
   * @param {string} scope
   * @param {string} [key]
   * @param {{ dataDir?: string }} [opts]
   */
  read(scope, key, opts = {}) {
    if (!scope || typeof scope !== "string") throw new Error("collection.read requires a scope");
    const doc = readScopeDoc(opts.dataDir, scope);
    if (key === undefined || key === null) return doc.entries || {};
    const entry = (doc.entries || {})[key];
    return entry ? entry.artifact : null;
  },

  /** List the keys present in a scope (handy for the ledger / shared-state assertions). */
  keys(scope, opts = {}) {
    const doc = readScopeDoc(opts.dataDir, scope);
    return Object.keys(doc.entries || {});
  },
};

export default collection;
