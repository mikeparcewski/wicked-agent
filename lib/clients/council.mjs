// wicked-agent — council client seam (ARCHITECTURE §8, §3 DISTRIBUTE).
//
// Reuse, do not re-implement: wicked-council owns agent-CLI discovery, the usability
// probe, and risk-convergence distribution. Batch D DRIVES the REAL council by
// shelling its built Rust binary (target/debug/wicked-council):
//   queue --topic <unit> --option <cli>... --criteria <kind> --session-id <sid>
//     -> { task_id, state, clis }       (convenes probed-usable CLIs; runs the fan-out)
//   poll  --task-id <id>
//     -> { state, verdict, ... }        (verdict.winning_recommendation = the pick)
//
// We frame the candidate CLIs as the council's OPTIONS and read the winning
// recommendation back as the assigned CLI. `queue` blocks until the worker finishes
// writing the ledger (council's CLI joins its thread), so a single poll after queue
// already sees the terminal state — but we poll in a short bounded loop for safety.
//
// GRACEFUL DEGRADE (the headline requirement): if the binary is missing, errors, or
// the council returns no usable verdict (state timed-out/failed, or a winner that
// isn't one of our candidates), we fall back to a DETERMINISTIC pick (first candidate)
// and set degraded:true. A distribution NEVER hard-fails the session.

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
// wicked-council is a READ-ONLY sibling; its built debug binary.
export const COUNCIL_BIN = resolve(
  __dirname,
  "../../../wicked-council/target/debug/wicked-council",
);

/** Run the council binary; returns { ok, json, stdout, stderr, code }. */
function runCouncil(args, opts = {}) {
  const r = spawnSync(COUNCIL_BIN, args, {
    encoding: "utf8",
    // Council's state dir keys off HOME; callers may isolate it per session.
    env: opts.env ? { ...process.env, ...opts.env } : process.env,
    timeout: opts.timeout ?? 60000,
  });
  if (r.error) return { ok: false, json: null, stdout: "", stderr: r.error.message, code: -1 };
  let json = null;
  try {
    json = JSON.parse(r.stdout || "");
  } catch {
    /* non-JSON stdout — leave null */
  }
  return { ok: r.status === 0, json, stdout: r.stdout || "", stderr: r.stderr || "", code: r.status };
}

export const council = {
  /** Is the council binary present + runnable? (degrade signal) */
  available(opts = {}) {
    if (!existsSync(COUNCIL_BIN)) return { available: false, reason: "binary not built" };
    const r = runCouncil(["health"], opts);
    return { available: r.ok && !!r.json?.ok, reason: r.ok ? undefined : r.stderr };
  },

  /**
   * Distribute a unit of work to the best CLI among `candidateClis`.
   * Drives the real council (queue + poll); degrades to the first candidate.
   *
   * @param {{ id?: string, description?: string }|string} unit  the unit (topic source).
   * @param {string[]} candidateClis  the CLIs framed as council options.
   * @param {{ criteria?: string[], sessionId?: string, env?: object, pollMs?: number, polls?: number }} [opts]
   * @returns {{ assignedCli: string, degraded: boolean, reason?: string, taskId?: string, verdictKind?: string, state?: string }}
   */
  distribute(unit, candidateClis = [], opts = {}) {
    const candidates = Array.isArray(candidateClis) ? candidateClis.filter(Boolean) : [];
    const fallback = candidates[0] ?? "claude";
    const topic =
      (typeof unit === "string" ? unit : unit?.description || unit?.id) || "distribute-work";
    const criteria = Array.isArray(opts.criteria) && opts.criteria.length ? opts.criteria : ["general"];
    const sessionId = opts.sessionId || "wicked-agent";

    if (candidates.length === 0) {
      return { assignedCli: fallback, degraded: true, reason: "no candidate CLIs given" };
    }

    // No binary → degrade immediately (never hard-fail).
    if (!existsSync(COUNCIL_BIN)) {
      return { assignedCli: fallback, degraded: true, reason: "council binary not built" };
    }

    // queue: frame candidate CLIs as options.
    const queueArgs = ["queue", "--topic", topic, "--criteria", criteria.join(","), "--session-id", sessionId];
    for (const c of candidates) queueArgs.push("--option", c);
    const q = runCouncil(queueArgs, { env: opts.env });
    const taskId = q.json?.task_id;
    if (!q.ok || !taskId) {
      return { assignedCli: fallback, degraded: true, reason: `council queue failed: ${q.stderr || q.code}` };
    }

    // poll until terminal (queue already blocks on the worker, so this is usually 1 read).
    const polls = Number.isInteger(opts.polls) ? opts.polls : 5;
    let status = null;
    for (let i = 0; i < polls; i++) {
      const p = runCouncil(["poll", "--task-id", taskId], { env: opts.env });
      status = p.json;
      const state = status?.state;
      if (state === "voted" || state === "timed-out" || state === "failed") break;
    }

    const state = status?.state;
    const winner = status?.verdict?.winning_recommendation;
    const verdictKind = status?.verdict?.kind;

    // A real, usable verdict whose winner is one of our candidates → use it.
    if (state === "voted" && typeof winner === "string") {
      const match = candidates.find((c) => c.toLowerCase() === winner.trim().toLowerCase());
      if (match) {
        return { assignedCli: match, degraded: false, taskId, verdictKind, state };
      }
      // Council voted but the winner isn't one of our candidate CLIs — degrade.
      return {
        assignedCli: fallback,
        degraded: true,
        reason: `council winner '${winner}' not among candidates`,
        taskId,
        verdictKind,
        state,
      };
    }

    // No usable verdict (timed-out / failed / no winner) → deterministic degrade.
    return {
      assignedCli: fallback,
      degraded: true,
      reason: `council returned no usable verdict (state=${state ?? "unknown"})`,
      taskId,
      state,
    };
  },
};

export default council;
