#!/usr/bin/env node
// wicked-agent-call — CLI dispatcher (Batch D: the harness integration).
//
// Stdlib only; the session loop and clients are pure ESM imports. Mirrors the
// wicked-governance / wicked-orchestration `<action> --key value` shape, JSON out
// (one object per line, cross-platform — no echo/printf \n quirks).
//
// Actions:
//   health                              -> { ok, app, version }, exit 0
//   run --file <problem.json>           -> run a full governed session (non-interactive)
//   status --session <id>               -> read the session ledger
//
// problem.json shape (run):
//   { "repos": ["./repo"], "clis": ["claude","gemini"], "problem": "do X. then Y.",
//     "entityMode": "shared" | "isolated",
//     "opts": { "dataDir": "...", "govDataDir": "...", "orchDataDir": "...", "vaultCwd": "..." } }

import { readFileSync } from "node:fs";

import { runSession } from "../lib/session.mjs";
import { ledger } from "../lib/ledger.mjs";

const VERSION = "0.1.0";
const APP = "wicked-agent";

function emit(obj) {
  process.stdout.write(JSON.stringify(obj) + "\n");
}

/** Parse argv into { action, flags } (--key value | --key=value | --key). */
function parseArgs(argv) {
  const [action, ...rest] = argv;
  const flags = {};
  for (let i = 0; i < rest.length; i++) {
    const tok = rest[i];
    if (tok && tok.startsWith("--") && tok.includes("=")) {
      const idx = tok.indexOf("=");
      flags[tok.slice(2, idx)] = tok.slice(idx + 1);
    } else if (tok && tok.startsWith("--")) {
      const key = tok.slice(2);
      const next = rest[i + 1];
      if (next !== undefined && !next.startsWith("--")) {
        flags[key] = next;
        i++;
      } else {
        flags[key] = true;
      }
    }
  }
  return { action, flags };
}

function readJsonFile(path) {
  return JSON.parse(readFileSync(path, "utf8"));
}

async function main() {
  const { action, flags } = parseArgs(process.argv.slice(2));

  try {
    if (action === "health") {
      emit({ ok: true, app: APP, version: VERSION });
      return 0;
    }

    // run — non-interactive: read a problem.json and run a full session.
    if (action === "run") {
      if (typeof flags.file !== "string") {
        throw new Error("run requires --file <problem.json>");
      }
      const spec = readJsonFile(flags.file);
      const result = await runSession({
        repos: spec.repos,
        clis: spec.clis,
        problem: spec.problem,
        entityMode: spec.entityMode,
        opts: spec.opts || {},
      });
      // Print a compact summary line (the full ledger is on disk).
      emit({
        ok: true,
        app: APP,
        action,
        session_id: result.session_id,
        status: result.status,
        rigor: result.rigor,
        approved: result.approved,
        rejected: result.rejected,
        units: result.units.map((u) => ({
          unit_id: u.unit_id,
          assigned_cli: u.assigned_cli,
          council_degraded: u.council_degraded,
          decision: u.decision ?? null,
          phase_status: u.phase_status,
          status: u.status,
          evidence_id: u.evidence_id ?? null,
        })),
      });
      return 0;
    }

    // status — read the session ledger for a session id.
    if (action === "status") {
      if (typeof flags.session !== "string") {
        throw new Error("status requires --session <id>");
      }
      const dataDir = typeof flags["data-dir"] === "string" ? flags["data-dir"] : undefined;
      const doc = ledger.getSession(flags.session, { dataDir });
      if (!doc) {
        emit({ ok: false, app: APP, action, error: `no such session '${flags.session}'` });
        return 1;
      }
      emit({ ok: true, app: APP, action, ...doc });
      return 0;
    }

    if (action === undefined) {
      process.stderr.write(
        `${APP}: no action given. usage: wicked-agent-call <health|run --file <f>|status --session <id>>\n`,
      );
      return 1;
    }

    process.stderr.write(
      `${APP}: unknown action "${action}". usage: wicked-agent-call <health|run|status>\n`,
    );
    return 1;
  } catch (e) {
    emit({ ok: false, app: APP, action, error: e.message });
    return 1;
  }
}

main().then((code) => process.exit(code));
