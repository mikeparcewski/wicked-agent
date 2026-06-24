// wicked-agent — governance client seam (ARCHITECTURE §8, §5; ADR-0003).
//
// Reuse, do not re-implement: wicked-governance owns the rigor/policy decision.
// Batch D DRIVES governance's real behavior by IMPORTING its lib (deterministic,
// no model on the decision path) and shelling its bin only for the evidence path:
//   - evaluate(context) -> select() + decide() -> a real ConformanceClaim.
//   - conform(claim)    -> governance's EvidencePort.record() -> vault evidence_id.
//
// Importing the governance lib (rather than shelling `evaluate`) is the right call
// for determinism (ADR-0003 §2: evaluate is pure) and keeps the gate cheap enough
// to fire on every unit. The conform path uses governance's own EvidencePort, which
// shells wicked-vault — the authoritative evidence store (we never record evidence
// ourselves).
//
// Degrade (honest, per ADR-0003 §6): if governance cannot be loaded, there is NO
// rigor gate — we surface { available:false } and the session loop refuses to claim
// rigor. If the vault is absent, evidence is pending but work still runs (§8).

import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
// wicked-governance is a READ-ONLY sibling; resolve its lib relative to this file.
const GOV_LIB = resolve(__dirname, "../../../wicked-governance/lib");

// Lazy, defensive ESM import so an absent sibling degrades to { available:false }
// instead of crashing the whole harness at module-load. Primed once via ensureLoaded().
let _loadError = null;
let _select, _decide, _EvidencePort;
let _ready = null;
async function ensureLoaded() {
  if (_ready) return _ready;
  _ready = (async () => {
    try {
      const selectMod = await import(`${GOV_LIB}/select.mjs`);
      const decideMod = await import(`${GOV_LIB}/decide.mjs`);
      const portMod = await import(`${GOV_LIB}/evidence-port.mjs`);
      _select = selectMod.select;
      _decide = decideMod.decide;
      _EvidencePort = portMod.EvidencePort;
      return { available: true };
    } catch (e) {
      _loadError = e;
      return { available: false, error: e.message };
    }
  })();
  return _ready;
}

export const governance = {
  /**
   * Is the governance sibling loadable? (the rigor-gate availability check, ADR-0003 §6)
   * @returns {Promise<{available:boolean, error?:string}>}
   */
  async available() {
    return ensureLoaded();
  },

  /**
   * Evaluate a unit's context against registered policies for its phase.
   * IMPORTS governance's pure select()+decide() — deterministic, no model call.
   * @param {object} context  the unit context; should carry `phase` and `scope`.
   * @param {{ dataDir?: string }} [opts]  governance policy data dir.
   * @returns {Promise<import("../../../wicked-governance/lib/evidence-port.mjs").ConformanceClaim>}
   */
  async evaluate(context = {}, opts = {}) {
    const r = await ensureLoaded();
    if (!r.available) {
      throw new Error(`governance unavailable: ${r.error || "load failed"}`);
    }
    const phase = typeof context.phase === "string" ? context.phase : undefined;
    if (!phase) throw new Error("governance.evaluate requires context.phase");
    const scope = context.scope;
    const selected = _select({ scope, phase, context, dataDir: opts.dataDir });
    // decide returns a real ConformanceClaim {claim_id, decision, obligations, ...}.
    return _decide(selected, { ...context, phase }, { scope });
  },

  /**
   * Record a ConformanceClaim as tamper-evident evidence via governance's
   * EvidencePort (which shells wicked-vault). Degrade: on any vault failure,
   * returns { evidence_id:null, degraded:true, reason } — work still runs (§8).
   * @param {object} claim  the ConformanceClaim from evaluate().
   * @param {{ vaultCwd?: string }} [opts]  roots the vault at a dir (test isolation).
   * @returns {Promise<{ evidence_id: string|null, recorded_via?: string, degraded?: boolean, reason?: string }>}
   */
  async conform(claim, opts = {}) {
    const r = await ensureLoaded();
    if (!r.available) {
      return { evidence_id: null, degraded: true, reason: `governance unavailable: ${r.error}` };
    }
    if (!claim || typeof claim !== "object") {
      throw new Error("governance.conform requires a claim object");
    }
    try {
      const port = new _EvidencePort({ vaultCwd: opts.vaultCwd });
      const rec = port.record(claim);
      return { evidence_id: rec.evidence_id, recorded_via: rec.recorded_via };
    } catch (e) {
      // Vault absent/failed → evidence pending; do NOT hard-fail the session.
      return { evidence_id: null, degraded: true, reason: e.message };
    }
  },
};

export default governance;
