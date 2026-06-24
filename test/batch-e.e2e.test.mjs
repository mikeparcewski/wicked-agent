// wicked-agent — Batch E E2E: soup-to-nuts with a REAL wrapped CLI doing REAL work.
//
// This is the DoD proof: a real subprocess (a wrapped "agent CLI") performs real work,
// GOVERNED + GATED + EVIDENCED through the WHOLE harness (session -> plan -> distribute ->
// execute=LAUNCH the wrapped CLI with the governance PreToolUse hook + orchestration phase
// brackets -> capture -> vault evidence -> session.completed).
//
// The wrapped CLI (test/fixtures/fake-agent-cli.mjs) is FAKE-but-REAL: given a TASK it performs
// a REAL action (writes an output file) AND declares its intended tool-call (Write <path> /
// Bash <command>) so governance can evaluate it. It honors the PreToolUse hook: a deny aborts
// the action before it happens.
//
// Two scenarios:
//   (1) CLEAN  — a real small task runs end-to-end. Asserts: the CLI's REAL artifact (a file)
//       exists on disk, governance ALLOWED the tool-call, the phase resolved to `approved`, and
//       a real vault evidence_id was recorded.
//   (2) GOVERNED-BLOCK — the CLI proposes a forbidden command (a secret export). Asserts: the
//       governance gate BLOCKS it (the pretool hook prevents the action), the phase resolved to
//       `rejected`, and the forbidden artifact/effect did NOT happen on disk.
//
// All sibling state is rooted in per-test temp dirs (no repo pollution). Governance/orchestration
// libs are IMPORTED by the clients; the wrapped CLI is a REAL node subprocess; vault is real
// (shelled by governance's EvidencePort) and rooted at an isolated vaultCwd.

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, existsSync, readFileSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { runSession } from "../lib/session.mjs";
import { ledger } from "../lib/ledger.mjs";
import { collection } from "../lib/clients/collection.mjs";
import { registerPolicy } from "../../wicked-governance/lib/store.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
// The fake-but-real wrapped CLI lives OUTSIDE test/ so `node --test` does not mis-discover it
// as a test file (it is a subprocess fixture, run by the harness, not a test).
const FAKE_CLI = resolve(__dirname, "../test-fixtures/fake-agent-cli.mjs");

let ROOT;
let dirs;

before(() => {
  ROOT = mkdtempSync(join(tmpdir(), "wicked-agent-e2e-e-"));
  dirs = {
    dataDir: join(ROOT, ".wicked-agent"),
    govDataDir: join(ROOT, ".wicked-governance"),
    orchDataDir: join(ROOT, ".wicked-orchestration"),
    vaultCwd: join(ROOT, ".vault"),
    councilHome: join(ROOT, "council-home"),
    work: join(ROOT, "work"),
  };
  // Deny policy: a tool-call touching a secret/API key in either unit phase is denied (high).
  // decide() tests trigger.contains as a regex over JSON.stringify(context); the wrapped CLI's
  // forbidden Bash command carries "API_KEY"/"secret", so the policy fires on the tool-call.
  for (const phase of ["unit-1", "unit-2"]) {
    registerPolicy(
      {
        id: `block-secrets-${phase}`,
        kind: "safety",
        effect: "deny",
        severity: "high",
        applies_to: [phase],
        trigger: { contains: "(?i)(api_key|secret|export\\s+\\w+=)" },
        criteria: "no secrets or credential exports handled by a wrapped CLI",
        rule: "deny any tool-call that exports a secret/API key",
      },
      { dataDir: dirs.govDataDir },
    );
  }
});

after(() => {
  if (ROOT && existsSync(ROOT)) rmSync(ROOT, { recursive: true, force: true });
});

// The wrapped CLI descriptor: a REAL launchable node subprocess in pretool-hook mode.
function wrappedClis(mode = "pretool-hook") {
  return {
    claude: { command: process.execPath, args: [FAKE_CLI], mode, launchable: true },
    gemini: { command: process.execPath, args: [FAKE_CLI], mode, launchable: true },
  };
}

function runOpts(sessionId, extra = {}) {
  return {
    sessionId,
    dataDir: dirs.dataDir,
    govDataDir: dirs.govDataDir,
    orchDataDir: dirs.orchDataDir,
    vaultCwd: dirs.vaultCwd,
    councilEnv: { HOME: dirs.councilHome },
    // Pin the wrapped CLI's working dirs so we can inspect the REAL artifacts on disk.
    wrappedWorkdir: join(dirs.work, sessionId),
    wrappedClis: wrappedClis(),
    ...extra,
  };
}

test("E2E soup-to-nuts: CLEAN task — real CLI writes a real file, ALLOWED, approved, evidenced", async () => {
  // A clean 1-unit problem (no secret words). The wrapped CLI will Write a real output file.
  const result = await runSession({
    repos: ["./demo-repo"],
    clis: ["claude", "gemini"],
    problem: "rotate the public deployment credentials note",
    entityMode: "shared",
    opts: runOpts("e2e-clean"),
  });

  assert.equal(result.rigor, true, "governance must be available so rigor is claimed");
  assert.equal(result.units.length, 1, "the clean problem decomposes into exactly 1 unit");

  const [u] = result.units;

  // ── It went through the REAL wrapped-CLI path (not the stub). ──
  assert.equal(u.wrapped, true, "the unit ran through inject.launchWrapped (real subprocess)");
  assert.equal(u.gate_timing, "pretool", "capable CLI used the pretool-hook governance gate");
  assert.ok(Array.isArray(u.tool_calls) && u.tool_calls.length >= 1, "the CLI surfaced a tool-call");
  assert.equal(u.tool_calls[0].tool, "Write", "the CLI's intended tool-call was a Write");
  assert.equal(u.cli_exit_code, 0, "the real subprocess exited 0 (work performed)");

  // ── governance ALLOWED the tool-call, the phase resolved to approved. ──
  assert.equal(u.decision, "allow", "governance allowed the clean tool-call");
  assert.equal(u.phase_status, "approved", "the orchestration phase resolved to approved");
  assert.equal(u.status, "done", "the unit's ledger status is done");
  assert.equal(u.blocked, false, "the clean unit was NOT blocked");

  // ── the wrapped CLI's REAL artifact (a file) exists ON DISK. ──
  assert.ok(typeof u.artifact_path === "string" && u.artifact_path.length > 0, "an artifact path was recorded");
  assert.ok(existsSync(u.artifact_path), `the real artifact file must exist on disk at ${u.artifact_path}`);
  const onDisk = readFileSync(u.artifact_path, "utf8");
  assert.match(onDisk, /^done: /, "the artifact file holds the CLI's real output");

  // ── the artifact was captured into the collection scope (d). ──
  const captured = collection.read(u.collection_scope, u.unit_id, { dataDir: dirs.dataDir });
  assert.ok(captured, "the artifact was captured into the collection scope");
  assert.equal(captured.unit_id, u.unit_id);
  assert.equal(captured.artifact_path, u.artifact_path, "captured artifact points at the real file");
  assert.match(captured.output, /^done: /, "captured artifact carries the real file content");

  // ── a REAL vault evidence_id was recorded (e). ──
  assert.ok(
    typeof u.evidence_id === "string" && u.evidence_id.length > 0,
    `a real vault evidence_id must be recorded, got ${u.evidence_id}`,
  );
  // And it verifies against the real vault (deterministic integrity re-derivation).
  const v = await governanceVerify(u.evidence_id);
  assert.equal(v.hash_ok, true, "the recorded evidence verifies (hash_ok) against the real vault");

  // ── the session ledger (OWNED projection) reflects the done outcome + pointers. ──
  const doc = ledger.getSession("e2e-clean", { dataDir: dirs.dataDir });
  assert.equal(doc.session.status, "completed", "session completed");
  const led = doc.work_units.find((w) => w.unit_id === u.unit_id);
  assert.equal(led.status, "done", "ledger reflects the unit done");
  assert.equal(led.evidence_id, u.evidence_id, "ledger carries the evidence_id pointer");
  assert.ok(led.phase_ref, "ledger points back to the orchestration phase");

  // ── the full session vocabulary fired soup-to-nuts. ──
  const types = result.events.map((e) => e.type);
  assert.equal(types[0], "wicked.agent.session.started");
  assert.ok(types.includes("wicked.agent.plan.created"));
  assert.ok(types.includes("wicked.agent.work.distributed"));
  assert.equal(types.filter((t) => t === "wicked.agent.task.completed").length, 1);
  assert.equal(types[types.length - 1], "wicked.agent.session.completed");
});

test("E2E soup-to-nuts: GOVERNED-BLOCK — forbidden tool-call BLOCKED by the gate, no effect", async () => {
  // A 1-unit problem instructing a secret export. The wrapped CLI PROPOSES the forbidden Bash
  // command; the pretool hook DENIES it; the CLI aborts; the effect never happens.
  const result = await runSession({
    repos: ["./demo-repo"],
    clis: ["claude", "gemini"],
    problem: "export the API_KEY secret to the deploy environment",
    entityMode: "shared",
    opts: runOpts("e2e-block"),
  });

  assert.equal(result.rigor, true, "governance must be available so the gate is real");
  assert.equal(result.units.length, 1, "the problem decomposes into exactly 1 unit");

  const [u] = result.units;

  // ── It went through the REAL wrapped-CLI path. ──
  assert.equal(u.wrapped, true, "the unit ran through inject.launchWrapped (real subprocess)");
  assert.equal(u.gate_timing, "pretool", "the pretool-hook gate was used");
  assert.ok(Array.isArray(u.tool_calls) && u.tool_calls.length >= 1, "the CLI surfaced its forbidden tool-call");
  assert.equal(u.tool_calls[0].tool, "Bash", "the CLI proposed a Bash command (the forbidden export)");
  assert.match(u.tool_calls[0].command, /API_KEY/, "the proposed command carries the secret");

  // ── the governance gate BLOCKED it: deny, phase rejected, unit rejected. ──
  assert.equal(u.blocked, true, "the gate BLOCKED the forbidden tool-call");
  assert.equal(u.decision, "deny", "governance denied the forbidden tool-call");
  assert.equal(u.phase_status, "rejected", "the orchestration phase resolved to rejected");
  assert.equal(u.status, "rejected", "the unit's ledger status is rejected");

  // ── the pretool hook prevented the action: the CLI exited non-zero, NO effect. ──
  assert.notEqual(u.cli_exit_code, 0, "the wrapped CLI aborted (non-zero exit) on the deny");

  // ── the forbidden artifact/effect did NOT happen on disk. ──
  const workdir = join(dirs.work, "e2e-block", u.unit_id);
  const leak = join(workdir, "secret-leak.txt");
  assert.ok(!existsSync(leak), `the forbidden file must NOT exist on disk (${leak})`);
  // Belt and suspenders: no file anywhere in the unit workdir holds the secret.
  if (existsSync(workdir)) {
    for (const name of readdirSync(workdir)) {
      const p = join(workdir, name);
      let body = "";
      try {
        body = readFileSync(p, "utf8");
      } catch {
        body = ""; // a dir (e.g. the hook dir) — skip content check
      }
      assert.ok(!/sk-live-DEADBEEF/.test(body), `no on-disk artifact may contain the secret (${p})`);
    }
  }

  // ── a blocked unit records NO evidence and writes NO artifact to the collection. ──
  assert.ok(!u.evidence_id, "a blocked unit records no evidence");
  assert.equal(u.artifact_written, false, "a blocked unit captures no artifact");
  const captured = collection.read(u.collection_scope, u.unit_id, { dataDir: dirs.dataDir });
  assert.equal(captured, null, "nothing forbidden landed in the collection scope");

  // ── the gate is STRUCTURAL: the denied phase carries gate_decision=deny on disk. ──
  const { getPhase } = await import("../../wicked-orchestration/lib/store.mjs");
  const doc = ledger.getSession("e2e-block", { dataDir: dirs.dataDir });
  const led = doc.work_units.find((w) => w.unit_id === u.unit_id);
  const phase = getPhase(led.phase_ref, { dataDir: dirs.orchDataDir });
  assert.equal(phase.status, "rejected", "orchestration's store shows the phase rejected");
  assert.equal(phase.gate_decision, "deny", "the denied phase carries the deny veto marker (structural)");
});

test("E2E: post-hoc governance ALSO fires the gate (degrade path) — rejects after the fact", async () => {
  // A CLI that CANNOT take a pretool hook (mode: post-hoc). It runs free and performs its
  // effect, then the harness evaluates its tool-calls AFTER the fact: a deny rejects the unit
  // and the forbidden effect is rolled back. The gate still fires — just later (ADR-0003 step 3).
  const result = await runSession({
    repos: ["./demo-repo"],
    clis: ["claude"],
    problem: "export the API_KEY secret post hoc",
    entityMode: "shared",
    opts: runOpts("e2e-posthoc", { wrappedClis: wrappedClis("post-hoc") }),
  });

  const [u] = result.units;
  assert.equal(u.wrapped, true, "ran through inject.launchWrapped");
  assert.equal(u.gate_timing, "post-hoc", "the post-hoc gate timing was used (capability degrade)");
  assert.equal(u.blocked, true, "post-hoc evaluate BLOCKED the forbidden tool-call after the fact");
  assert.equal(u.decision, "deny", "post-hoc decision is deny");
  assert.equal(u.phase_status, "rejected", "the phase resolved to rejected via the post-hoc gate");

  // The CLI DID run its effect (post-hoc = no pre-tool gate), but the harness rolled it back.
  const workdir = join(dirs.work, "e2e-posthoc", u.unit_id);
  const leak = join(workdir, "secret-leak.txt");
  assert.ok(!existsSync(leak), "post-hoc: the forbidden effect was rolled back (file removed)");
  assert.ok(!u.evidence_id, "post-hoc blocked unit records no approval evidence");
});

// Re-derive a recorded evidence id's integrity via governance's EvidencePort (real vault).
async function governanceVerify(evidenceId) {
  const { EvidencePort } = await import("../../wicked-governance/lib/evidence-port.mjs");
  const port = new EvidencePort({ vaultCwd: dirs.vaultCwd });
  return port.verify(evidenceId);
}
