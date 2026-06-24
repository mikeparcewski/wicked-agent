// wicked-agent — Batch D E2E: the WHOLE integration (node:test, stdlib only).
//
// Runs a real session that DRIVES the three sibling apps and proves the headline
// invariant: the GOVERNANCE GATE FIRES THROUGH THE AGENT on every unit.
//
// Setup: a governance deny policy targeting phase "unit-1". A 2-unit problem where
//   - unit-1's context triggers the deny  => its orchestration phase MUST resolve to
//     `rejected` (governance gate enforced through the agent, structurally), and
//   - unit-2's context is clean           => its phase MUST resolve to `approved`,
//     an evidence_id MUST be recorded, and an artifact MUST land in the collection scope.
// Also asserts: distribution happened for both units (council real or degraded); the
// session ledger reflects both outcomes; entityMode:"shared" puts both units in ONE
// collection scope, vs distinct scopes under "isolated".
//
// All sibling state is rooted in per-test temp dirs (no repo pollution, no shared
// global state). Governance/orchestration libs are IMPORTED by the clients; the
// council Rust binary is SHELLED (real) and degrades cleanly if it yields no verdict.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { runSession } from "../lib/session.mjs";
import { ledger } from "../lib/ledger.mjs";
import { collection } from "../lib/clients/collection.mjs";
import { registerPolicy } from "../../wicked-governance/lib/store.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const COUNCIL_BIN = resolve(__dirname, "../../wicked-council/target/debug/wicked-council");

let ROOT;
let dirs;

before(() => {
  ROOT = mkdtempSync(join(tmpdir(), "wicked-agent-e2e-"));
  dirs = {
    dataDir: join(ROOT, ".wicked-agent"),
    govDataDir: join(ROOT, ".wicked-governance"),
    orchDataDir: join(ROOT, ".wicked-orchestration"),
    vaultCwd: join(ROOT, ".vault"),
    councilHome: join(ROOT, "council-home"),
  };
  // Register the deny policy: any unit whose phase is "unit-1" AND whose context
  // contains "secret" is denied (high severity). decide() tests trigger.contains as
  // a regex over JSON.stringify(context), so the unit-1 description carries "secret".
  registerPolicy(
    {
      id: "block-secrets",
      kind: "safety",
      effect: "deny",
      severity: "high",
      applies_to: ["unit-1"],
      trigger: { contains: "(?i)secret" },
      criteria: "no secrets handled in unit-1",
      rule: "deny any unit-1 work touching secrets",
    },
    { dataDir: dirs.govDataDir },
  );
});

after(() => {
  if (ROOT && existsSync(ROOT)) rmSync(ROOT, { recursive: true, force: true });
});

function runOpts(sessionId, extra = {}) {
  return {
    sessionId,
    dataDir: dirs.dataDir,
    govDataDir: dirs.govDataDir,
    orchDataDir: dirs.orchDataDir,
    vaultCwd: dirs.vaultCwd,
    // Isolate council's state dir (it keys off HOME) so the E2E never touches the
    // user's real council ledger.
    councilEnv: { HOME: dirs.councilHome },
    ...extra,
  };
}

// The 2-unit problem. planUnits splits on sentence terminators: unit-1 mentions a
// "secret" (triggers deny), unit-2 is clean.
const PROBLEM = "rotate the secret API token. write the public changelog entry.";

test("E2E: governance gate fires THROUGH the agent — deny rejects unit-1, clean approves unit-2", async () => {
  const result = await runSession({
    repos: ["./demo-repo"],
    clis: ["claude", "gemini"],
    problem: PROBLEM,
    entityMode: "shared",
    opts: runOpts("e2e-shared"),
  });

  // Rigor was claimable (governance loaded) — the gate is real, not skipped.
  assert.equal(result.rigor, true, "governance must be available so rigor is claimed");
  assert.equal(result.units.length, 2, "problem must decompose into exactly 2 units");

  const [u1, u2] = result.units;

  // ── unit-1: the deny policy fired -> phase rejected THROUGH the agent. ──
  assert.equal(u1.decision, "deny", "unit-1 context must trigger the deny policy");
  assert.equal(u1.phase_status, "rejected", "unit-1 orchestration phase must resolve to rejected");
  assert.equal(u1.status, "rejected", "unit-1 ledger status must be rejected");
  assert.equal(u1.artifact_written, false, "a rejected unit writes NO artifact");
  assert.ok(!u1.evidence_id, "a rejected unit records NO evidence");

  // ── unit-2: clean -> approved + evidence recorded + artifact captured. ──
  assert.equal(u2.decision, "allow", "unit-2 clean context must be allowed");
  assert.equal(u2.phase_status, "approved", "unit-2 orchestration phase must resolve to approved");
  assert.equal(u2.status, "done", "unit-2 ledger status must be done");
  assert.ok(
    typeof u2.evidence_id === "string" && u2.evidence_id.length > 0,
    `unit-2 must record a vault evidence_id, got ${u2.evidence_id}`,
  );

  // The approved unit's artifact actually landed in the collection scope.
  const artifact = collection.read(u2.collection_scope, u2.unit_id, { dataDir: dirs.dataDir });
  assert.ok(artifact, "unit-2 artifact must be present in the collection scope");
  assert.equal(artifact.unit_id, u2.unit_id);

  // The rejected unit's artifact must NOT be in the scope.
  const denied = collection.read(u1.collection_scope, u1.unit_id, { dataDir: dirs.dataDir });
  assert.equal(denied, null, "unit-1 (denied) must not appear in the collection scope");

  // ── distribution happened for BOTH units (council real OR degraded). ──
  for (const u of result.units) {
    assert.ok(typeof u.assigned_cli === "string" && u.assigned_cli.length > 0, "each unit gets an assigned CLI");
    assert.ok(["claude", "gemini"].includes(u.assigned_cli), "assignment is one of the candidate CLIs");
    assert.equal(typeof u.council_degraded, "boolean", "council outcome (real/degraded) is recorded");
  }
  const distributed = result.events.filter((e) => e.type === "wicked.agent.work.distributed");
  assert.equal(distributed.length, 2, "a distribution event fired for each unit");

  // ── the full session vocabulary was emitted in order. ──
  const types = result.events.map((e) => e.type);
  assert.deepEqual(
    [
      types[0],
      types.includes("wicked.agent.plan.created"),
      types.filter((t) => t === "wicked.agent.task.completed").length,
      types[types.length - 1],
    ],
    ["wicked.agent.session.started", true, 2, "wicked.agent.session.completed"],
    "session.started ... plan.created ... 2x task.completed ... session.completed",
  );

  // ── the session ledger (the OWNED projection) reflects BOTH outcomes. ──
  const doc = ledger.getSession("e2e-shared", { dataDir: dirs.dataDir });
  assert.ok(doc, "session ledger doc exists");
  assert.equal(doc.session.status, "completed", "session completed");
  assert.equal(doc.work_units.length, 2, "ledger has both work units");
  const ledU1 = doc.work_units.find((u) => u.unit_id === u1.unit_id);
  const ledU2 = doc.work_units.find((u) => u.unit_id === u2.unit_id);
  assert.equal(ledU1.status, "rejected", "ledger reflects unit-1 rejected");
  assert.equal(ledU1.decision, "deny");
  assert.equal(ledU2.status, "done", "ledger reflects unit-2 done");
  assert.equal(ledU2.evidence_id, u2.evidence_id, "ledger carries the evidence_id pointer");
  // Each unit points back to its authoritative orchestration phase.
  assert.ok(ledU1.phase_ref && ledU2.phase_ref, "work units point back to orchestration phases");

  assert.equal(result.approved, 1, "exactly one unit approved");
  assert.equal(result.rejected, 1, "exactly one unit rejected");
});

test("E2E: the gate is STRUCTURAL — the denied phase carries gate_decision=deny on disk", async () => {
  // Read orchestration's authoritative phase projection directly: the denied phase
  // must persist gate_decision=deny (the reducer's hard veto marker, ADR-0003), not
  // merely be labelled rejected by the agent's own bookkeeping.
  const { getPhase } = await import("../../wicked-orchestration/lib/store.mjs");
  const doc = ledger.getSession("e2e-shared", { dataDir: dirs.dataDir });
  const u1 = doc.work_units.find((u) => u.unit_id === "e2e-shared:u1");
  const u2 = doc.work_units.find((u) => u.unit_id === "e2e-shared:u2");

  const p1 = getPhase(u1.phase_ref, { dataDir: dirs.orchDataDir });
  const p2 = getPhase(u2.phase_ref, { dataDir: dirs.orchDataDir });
  assert.equal(p1.status, "rejected", "denied phase status is rejected in orchestration's store");
  assert.equal(p1.gate_decision, "deny", "denied phase carries the deny veto marker (structural)");
  assert.equal(p2.status, "approved", "clean phase status is approved in orchestration's store");
  assert.equal(p2.gate_decision, "allow", "clean phase carries the allow verdict");
});

test("E2E: entityMode shared = ONE scope for both units; isolated = DISTINCT scopes", async () => {
  // Shared run (reuse the first run's ledger).
  const shared = ledger.getSession("e2e-shared", { dataDir: dirs.dataDir });
  const sharedScopes = new Set(shared.work_units.map((u) => u.collection_scope));
  assert.equal(sharedScopes.size, 1, "shared mode: both units write ONE collection scope");

  // Isolated run with a clean policy dir (no deny) so both units approve and we can
  // compare their scopes directly.
  const isoGov = join(ROOT, ".gov-empty");
  const isoResult = await runSession({
    repos: ["./demo-repo"],
    clis: ["claude", "gemini"],
    problem: "build the clean alpha component. build the clean beta component.",
    entityMode: "isolated",
    opts: runOpts("e2e-isolated", { govDataDir: isoGov }),
  });

  // Top-level shared collection_scope is null under isolated (per-unit, not one).
  assert.equal(isoResult.collection_scope, null, "isolated has no single shared scope");
  const isoScopes = isoResult.units.map((u) => u.collection_scope);
  assert.equal(new Set(isoScopes).size, isoScopes.length, "isolated mode: each unit gets a DISTINCT scope");
  // And distinct from the shared scope shape.
  const sharedScope = [...sharedScopes][0];
  for (const s of isoScopes) {
    assert.notEqual(s, sharedScope, "isolated unit scope differs from the shared scope");
  }
});

test("E2E: council binary is present and runnable (drives real distribution path)", () => {
  // The integration shells the real council binary; assert it was built. (Distribution
  // still degrades gracefully if it yields no verdict — proven by council_degraded above.)
  assert.ok(existsSync(COUNCIL_BIN), `council binary must be built at ${COUNCIL_BIN}`);
});
