// wicked-agent — orchestration client seam (ARCHITECTURE §8, §3).
//
// Reuse, do not re-implement: wicked-orchestration owns workflow state, phases,
// gates, and the state machine. Batch D DRIVES it by IMPORTING its lib — a
// wicked-agent session IS a workflow; each unit of work IS a phase with a gate.
// We never invent a state machine; we walk orchestration's real one through its
// single-writer reducer (applyEvent) and its enforceable gate (applyGate).
//
// The phase lifecycle we drive per unit (ARCHITECTURE §3 EXECUTE):
//   openPhase           -> pending
//   advancePhase        -> in_progress       (emits wicked.phase.started)
//                       -> ready_for_gate     (emits wicked.phase.ready-for-gate)
//                       -> gate_running       (no event; the gate is about to fire)
//   gate(phaseId, claim)-> applyGate -> {approved|approved_with_conditions|rejected}
//
// The governance claim GATES the unit: a deny/reject claim STRUCTURALLY blocks the
// approved edge (orchestration's gate_decision veto, ADR-0003), so a denied unit's
// phase resolves to `rejected` — enforced THROUGH the agent.
//
// Degrade (§8): if orchestration cannot be loaded, the session loop falls back to
// local gating (no event backbone) — surfaced honestly via { available:false }.

import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { randomUUID } from "node:crypto";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ORCH_LIB = resolve(__dirname, "../../../wicked-orchestration/lib");

let _loadError = null;
let _createWorkflow, _openPhase, _getPhase, _applyEvent, _applyGate, _phaseIdFor;
let _ready = null;
async function ensureLoaded() {
  if (_ready) return _ready;
  _ready = (async () => {
    try {
      const store = await import(`${ORCH_LIB}/store.mjs`);
      const reducer = await import(`${ORCH_LIB}/reducer.mjs`);
      const gate = await import(`${ORCH_LIB}/gate.mjs`);
      _createWorkflow = store.createWorkflow;
      _openPhase = store.openPhase;
      _getPhase = store.getPhase;
      _phaseIdFor = store.phaseIdFor;
      _applyEvent = reducer.applyEvent;
      _applyGate = gate.applyGate;
      return { available: true };
    } catch (e) {
      _loadError = e;
      return { available: false, error: e.message };
    }
  })();
  return _ready;
}

export const orchestration = {
  /** @returns {Promise<{available:boolean, error?:string}>} */
  async available() {
    return ensureLoaded();
  },

  /**
   * Start (or idempotently return) the workflow that backs a session.
   * @param {{ id: string, name?: string, scope?: string, dataDir?: string }} args
   */
  async startWorkflow({ id, name, scope, dataDir } = {}) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    return _createWorkflow({ id, name, scope, correlationId: id, dataDir });
  },

  /**
   * Open an orchestration phase for a unit of work (status: pending).
   * @param {{ workflowId: string, name: string, seq?: number, dataDir?: string }} args
   * @returns {Promise<object>} the phase row (carries phase_id).
   */
  async openPhase({ workflowId, name, seq, dataDir } = {}) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    return _openPhase({ workflowId, name, seq, dataDir });
  },

  /**
   * Walk a phase from `pending` up to `gate_running` so the gate can fire.
   * Each step is one reducer transition (the real state machine). Idempotent ids
   * are derived per (phaseId, to) so a re-run is a no-op duplicate, not a crash.
   * @param {string} phaseId
   * @param {{ dataDir?: string }} [opts]
   * @returns {Promise<{ phase: object, transitions: string[] }>}
   */
  async advancePhase(phaseId, opts = {}) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    const dataDir = opts.dataDir;
    const ladder = ["in_progress", "ready_for_gate", "gate_running"];
    const transitions = [];
    for (const to of ladder) {
      const res = _applyEvent(
        { id: `agent:${phaseId}:${to}:${randomUUID()}`, phaseId, to },
        { dataDir },
      );
      if (res.applied) transitions.push(to);
      else if (res.reason && res.reason.startsWith("illegal_transition")) {
        // Already past this rung (idempotent re-run) — keep walking.
        continue;
      }
    }
    return { phase: _getPhase(phaseId, { dataDir }), transitions };
  },

  /**
   * Fire the phase gate from a governance ConformanceClaim. Delegates to
   * orchestration's applyGate: deny/reject -> rejected (approved structurally
   * vetoed), allow_with_conditions -> approved_with_conditions, allow -> approved.
   * @param {string} phaseId  a phase currently in `gate_running`.
   * @param {object} claim     the ConformanceClaim (carries `decision`, `obligations`).
   * @param {{ dataDir?: string }} [opts]
   * @returns {Promise<{ resolved: string, applied: boolean, phase: object|null, obligations: string[], conditions: boolean, reason?: string }>}
   */
  async gate(phaseId, claim, opts = {}) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    return _applyGate(phaseId, claim, { dataDir: opts.dataDir });
  },

  /** Read a phase row by id (or null). */
  async getPhase(phaseId, opts = {}) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    return _getPhase(phaseId, { dataDir: opts.dataDir });
  },

  /** Deterministic phase id from (workflowId, name) — re-open is idempotent. */
  async phaseIdFor(workflowId, name) {
    const r = await ensureLoaded();
    if (!r.available) throw new Error(`orchestration unavailable: ${r.error}`);
    return _phaseIdFor(workflowId, name);
  },
};

export default orchestration;
