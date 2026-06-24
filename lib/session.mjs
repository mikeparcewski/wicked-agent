// wicked-agent — the session loop (ARCHITECTURE §3: the OWNED interactive flow).
//
// runSession DRIVES the three sibling apps through plan -> distribute -> execute,
// under the estate's rigor. wicked-agent owns ONLY this loop, the per-CLI scope
// assignment, and the ledger; everything else is reused:
//   - council        -> picks the CLI per unit (real binary or graceful degrade)
//   - orchestration   -> each unit is a phase with a gate (the event backbone)
//   - governance      -> evaluates each unit's context; the claim GATES the phase
//   - collection      -> shared (or isolated) scope makes N CLIs one entity
//   - vault (via gov) -> tamper-evident evidence of each approved unit
//
// THE INVARIANT (ADR-0003): the governance gate fires on EVERY unit. A deny/reject
// claim STRUCTURALLY blocks that unit's approval (orchestration's gate_decision veto)
// — engagement governs the reaction, not whether the gate fires. The E2E proves a
// denied unit's phase resolves to `rejected` THROUGH this loop.

import { randomUUID, createHash } from "node:crypto";

import { resolveScope } from "./scope.mjs";
import { ledger } from "./ledger.mjs";
import { governance } from "./clients/governance.mjs";
import { orchestration } from "./clients/orchestration.mjs";
import { council } from "./clients/council.mjs";
import { collection } from "./clients/collection.mjs";
import { launchWrapped } from "./inject.mjs";

/**
 * Resolve a launchable wrapped-CLI descriptor for an assigned CLI, if one is configured.
 * ADDITIVE (Batch E): the execute step uses lib/inject.launchWrapped (a REAL subprocess +
 * governance hook + capture + evidence) ONLY when the assigned CLI maps to a launchable
 * descriptor here; otherwise it keeps the deterministic stub path (so Batch D stays green).
 *
 * A descriptor is supplied via opts.wrappedClis — either a map {cliId -> descriptor} or an
 * array of descriptors carrying an `id`. A descriptor is "launchable" when it has a
 * `command` (the executable) and is flagged `launchable !== false`.
 * @returns {object|null} the launchable descriptor (with id defaulted) or null.
 */
function resolveWrappedCli(cliId, opts = {}) {
  const cfg = opts.wrappedClis;
  if (!cfg) return null;
  let desc = null;
  if (Array.isArray(cfg)) desc = cfg.find((c) => c && (c.id === cliId || c.cli_id === cliId)) || null;
  else if (typeof cfg === "object") desc = cfg[cliId] || null;
  if (!desc || typeof desc.command !== "string" || desc.launchable === false) return null;
  return { id: cliId, ...desc };
}

/** A deterministic short id from parts (no ulid dep; matches the estate's style). */
function shortId(...parts) {
  return createHash("sha256").update(parts.join("|")).digest("hex").slice(0, 16);
}

/**
 * Decompose a free-text problem into ordered work units. A simple deterministic
 * split (the plan is wicked-agent's; ordering/gating is orchestration's). Splits on
 * newlines / sentence terminators / semicolons; falls back to one unit.
 * @param {string} problem
 * @param {string} sessionId
 * @returns {{ unit_id: string, ord: number, description: string }[]}
 */
export function planUnits(problem, sessionId) {
  const text = typeof problem === "string" ? problem : "";
  const pieces = text
    .split(/\n+|(?<=[.!?])\s+|;\s*/)
    .map((s) => s.trim())
    .filter(Boolean);
  const descs = pieces.length ? pieces : [text.trim() || "unit"];
  return descs.map((description, i) => ({
    unit_id: `${sessionId}:u${i + 1}`,
    ord: i + 1,
    description,
  }));
}

/**
 * Run a full governed session.
 *
 * @param {object} args
 * @param {string[]} args.repos        repo(s) that become the session scope.
 * @param {string[]} args.clis         the agent CLIs to wrap (council options).
 * @param {string}   args.problem      free-text problem; decomposed into units.
 * @param {"shared"|"isolated"} [args.entityMode="shared"]  scope toggle (§6).
 * @param {object}   [args.opts]       wiring overrides:
 *   - dataDir       : wicked-agent ledger + collection dir (default ./.wicked-agent)
 *   - govDataDir    : governance policy dir (default ./.wicked-governance)
 *   - orchDataDir   : orchestration projection dir (default ./.wicked-orchestration)
 *   - vaultCwd      : vault root for evidence (test isolation)
 *   - councilEnv    : env overrides for the council binary (e.g. isolated HOME)
 *   - emit          : (eventType, payload) => void   bus emitter (fire-and-forget)
 * @returns {Promise<object>}  the session result (mirrors the ledger doc + events).
 */
export async function runSession({ repos, clis, problem, entityMode = "shared", opts = {} } = {}) {
  const sessionId = opts.sessionId || `sess-${shortId(String(repos), String(problem), randomUUID())}`;
  const workflowId = `wf-${sessionId}`;
  const dataDir = opts.dataDir;
  const repoList = Array.isArray(repos) ? repos : repos ? [repos] : [];
  const cliList = Array.isArray(clis) && clis.length ? clis : ["claude"];
  const mode = entityMode === "isolated" ? "isolated" : "shared";

  const events = [];
  const emit = (type, payload) => {
    const ev = { type, payload, at: new Date().toISOString() };
    events.push(ev);
    try {
      if (typeof opts.emit === "function") opts.emit(type, payload);
    } catch {
      /* bus is fire-and-forget; never let emission break the loop */
    }
  };

  // ── Rigor availability (ADR-0003 §6): no governance -> no rigor claim. ──
  const govStatus = await governance.available();
  const orchStatus = await orchestration.available();
  const councilStatus = council.available({ env: opts.councilEnv });
  const rigor = govStatus.available === true;

  // ── 1. session.started + pin scope(s) ──
  // Shared: one collection+vault scope for ALL CLIs. Isolated: per-CLI scope.
  const sharedScope = resolveScope({ entityMode: mode, sessionId, cliId: cliList[0] });
  const collectionScope = mode === "shared" ? sharedScope.scope : null; // per-CLI when isolated
  const vaultScope = `wicked-agent/${sessionId}`;

  ledger.createSession(
    {
      session_id: sessionId,
      workflow_id: workflowId,
      repos: JSON.stringify(repoList),
      problem,
      entity_mode: mode,
      collection_scope: collectionScope, // null under isolated (per-CLI; see wrapped_clis)
      vault_scope: vaultScope,
      status: "planning",
      rigor_claimed: rigor,
    },
    { dataDir },
  );

  emit("wicked.agent.session.started", {
    session_id: sessionId,
    workflow_id: workflowId,
    repos: repoList,
    entity_mode: mode,
    rigor: rigor ? "governed" : "ungoverned (governance absent)",
  });

  // Start the orchestration workflow that backs the session (if available).
  if (orchStatus.available) {
    await orchestration.startWorkflow({
      id: workflowId,
      name: `agent session ${sessionId}`,
      scope: vaultScope,
      dataDir: opts.orchDataDir,
    });
  }

  // Record each wrapped CLI + its resolved scope (the §6 toggle made concrete).
  for (const cliId of cliList) {
    const sc = resolveScope({ entityMode: mode, sessionId, cliId });
    ledger.recordWrappedCli(
      sessionId,
      {
        cli_id: cliId,
        probe_ref: councilStatus.available ? "council:registry" : null,
        // Capability matrix: skeleton injection layer (lib/inject.mjs) lands separately;
        // we record honest unknowns rather than fabricate support flags.
        supports_skills: null,
        supports_hook: null,
        supports_env: null,
        governance_mode: rigor ? "pretool-hook" : "none",
        scope: sc.scope,
      },
      { dataDir },
    );
  }

  // ── 2. PLAN ──
  const units = planUnits(problem, sessionId);
  ledger.updateSession(sessionId, { status: "distributing" }, { dataDir });
  emit("wicked.agent.plan.created", {
    session_id: sessionId,
    units: units.map((u) => ({ unit_id: u.unit_id, ord: u.ord, description: u.description })),
  });
  for (const u of units) {
    ledger.upsertWorkUnit(sessionId, { ...u, session_id: sessionId, status: "pending" }, { dataDir });
  }

  // ── 3. DISTRIBUTE (per unit) ──
  for (const u of units) {
    const dist = council.distribute(u, cliList, {
      sessionId,
      criteria: ["general"],
      env: opts.councilEnv,
    });
    u.assigned_cli = dist.assignedCli;
    u.council = dist;
    ledger.upsertWorkUnit(
      sessionId,
      {
        unit_id: u.unit_id,
        assigned_cli: dist.assignedCli,
        council_verdict_ref: dist.taskId || null,
        council_degraded: dist.degraded,
        status: "distributed",
      },
      { dataDir },
    );
    emit("wicked.agent.work.distributed", {
      session_id: sessionId,
      unit_id: u.unit_id,
      assigned_cli: dist.assignedCli,
      council_degraded: dist.degraded,
      council_state: dist.state,
    });
  }

  // ── 4. EXECUTE (per unit): phase -> work -> evaluate -> conform -> gate ──
  ledger.updateSession(sessionId, { status: "executing" }, { dataDir });

  for (const u of units) {
    const phaseName = `unit-${u.ord}`;
    const result = {
      unit_id: u.unit_id,
      ord: u.ord,
      assigned_cli: u.assigned_cli,
      council_degraded: u.council?.degraded ?? null,
    };

    // The collection scope this unit's CLI writes to (shared vs isolated, §6).
    //   shared   -> the ONE session scope (resolveScope shared): N CLIs, one entity.
    //   isolated -> each unit runs as an independent mini-session (ADR-0001: "separate
    //               sessions that happen to share a plan"), so it gets its OWN scope.
    //               We qualify the per-CLI isolated base with the unit id so two units
    //               are genuinely isolated even when council assigns them the same CLI.
    const baseScope = resolveScope({ entityMode: mode, sessionId, cliId: u.assigned_cli }).scope;
    const unitScope = mode === "shared" ? baseScope : `${baseScope}/${u.unit_id}`;
    result.collection_scope = unitScope;

    // 4a. open an orchestration phase + walk it to gate_running.
    let phaseId = null;
    if (orchStatus.available) {
      const phase = await orchestration.openPhase({
        workflowId,
        name: phaseName,
        seq: u.ord,
        dataDir: opts.orchDataDir,
      });
      phaseId = phase.phase_id;
      await orchestration.advancePhase(phaseId, { dataDir: opts.orchDataDir });
    }
    result.phase_ref = phaseId;

    // 4b/4c/4d. Do the unit's work, gate it, capture + evidence. Two paths (ADDITIVE):
    //   (E) a REAL launchable wrapped CLI -> lib/inject.launchWrapped: a real subprocess
    //       does the work with the governance hook wired (pretool) or post-hoc evaluate, its
    //       artifact captured to the collection scope and evidence recorded to vault. The
    //       orchestration phase gate STILL fires here on the launch's claim (the invariant).
    //   (D) otherwise -> the deterministic stub path (unchanged; keeps Batch D green).
    // Either way: rigor=false means no governance -> no rigor claim (surfaced honestly).
    const wrapped = rigor ? resolveWrappedCli(u.assigned_cli, opts) : null;

    let claim = null;
    let gateOutcome = null;
    let evidence = { evidence_id: null };

    if (wrapped) {
      // ── (E) LAUNCH the real wrapped CLI under the governance hook + phase brackets. ──
      const launch = await launchWrapped(wrapped, u, {
        scope: unitScope,
        phaseName,
        vaultScope,
        governanceClient: governance,
        collectionClient: collection,
        vault: { vaultCwd: opts.vaultCwd },
        govDataDir: opts.govDataDir,
        dataDir,
        workdir: opts.wrappedWorkdir ? `${opts.wrappedWorkdir}/${u.unit_id}` : undefined,
      });
      claim = launch.claim;
      result.conformance_ref = claim?.claim_id ?? null;
      result.decision = launch.decision;
      result.wrapped = true;
      result.gate_timing = launch.gate_timing; // 'pretool' | 'post-hoc' (ADR-0003)
      result.tool_calls = launch.tool_calls;
      result.artifact_path = launch.artifact_path;
      result.cli_exit_code = launch.exit_code;

      // The phase gate fires THROUGH orchestration on the launch's claim (the invariant).
      // launchWrapped already wired the gate onto the subprocess; here we make orchestration's
      // phase status authoritative too (so the structural gate_decision marker is on disk).
      if (claim && orchStatus.available && phaseId) {
        gateOutcome = await orchestration.gate(phaseId, claim, { dataDir: opts.orchDataDir });
        result.phase_status = gateOutcome.phase?.status ?? null;
        result.gate_resolved = gateOutcome.resolved;
      } else if (claim) {
        // Orchestration absent: local gating from the launch's claim (a material cut).
        result.phase_status =
          launch.blocked
            ? "rejected"
            : claim.decision === "allow_with_conditions"
              ? "approved_with_conditions"
              : "approved";
        result.gate_resolved = result.phase_status;
        result.local_gating = true;
      } else {
        // The CLI surfaced no tool-call to gate — treat as nothing-approved (no claim, no work).
        result.phase_status = launch.blocked ? "rejected" : "approved";
        result.gate_resolved = result.phase_status;
      }

      // launchWrapped owns capture (d) + evidence (e) — gated on the SAME blocked verdict.
      result.artifact_written = !launch.blocked;
      result.evidence_id = launch.evidence_id ?? null;
      result.blocked = launch.blocked;
      result.status = launch.blocked ? "rejected" : "done";
    } else {
      // ── (D) the deterministic STUB path (Batch D) — unchanged. ──
      // 4b. run the (stubbed) unit work — produce an output artifact.
      const workArtifact = {
        unit_id: u.unit_id,
        description: u.description,
        assigned_cli: u.assigned_cli,
        output: `stub-output for ${u.description}`,
      };

      // 4c. governance.evaluate the unit's context (the gate INPUT). The phase name is
      //     the governance `phase` so a policy's applies_to can target this unit-kind.
      if (rigor) {
        const context = {
          phase: phaseName,
          scope: vaultScope,
          unit_id: u.unit_id,
          description: u.description,
          assigned_cli: u.assigned_cli,
          // The unit's work, so a deny policy can trigger on its content.
          work: workArtifact.output,
        };
        claim = await governance.evaluate(context, { dataDir: opts.govDataDir });
        result.conformance_ref = claim.claim_id;
        result.decision = claim.decision;

        // 4c'. governance gate fires THROUGH orchestration (the invariant).
        if (orchStatus.available && phaseId) {
          gateOutcome = await orchestration.gate(phaseId, claim, { dataDir: opts.orchDataDir });
          result.phase_status = gateOutcome.phase?.status ?? null;
          result.gate_resolved = gateOutcome.resolved;
        } else {
          // Orchestration absent: local gating (a material cut, surfaced). The claim's
          // decision IS the verdict; we map it the same way orchestration's gate would.
          result.phase_status =
            claim.decision === "deny" || claim.decision === "reject"
              ? "rejected"
              : claim.decision === "allow_with_conditions"
                ? "approved_with_conditions"
                : "approved";
          result.gate_resolved = result.phase_status;
          result.local_gating = true;
        }
      } else {
        // No governance -> no rigor gate. Refuse to claim it; the phase cannot approve
        // on rigor grounds. Surface honestly.
        result.phase_status = "gate_running";
        result.gate_resolved = "gate_running";
        result.ungoverned = true;
      }

      const approved =
        result.phase_status === "approved" || result.phase_status === "approved_with_conditions";

      // 4d. on approval: capture the artifact to the collection scope + record evidence.
      if (approved) {
        collection.write(unitScope, u.unit_id, workArtifact, { dataDir });
        result.artifact_written = true;
        if (rigor && claim) {
          evidence = await governance.conform(claim, { vaultCwd: opts.vaultCwd });
          result.evidence_id = evidence.evidence_id;
          result.evidence_degraded = evidence.degraded ?? false;
        }
        result.status = "done";
      } else {
        result.artifact_written = false;
        result.status = "rejected";
      }
    }

    ledger.upsertWorkUnit(
      sessionId,
      {
        unit_id: u.unit_id,
        phase_ref: result.phase_ref,
        conformance_ref: result.conformance_ref ?? null,
        decision: result.decision ?? null,
        phase_status: result.phase_status,
        evidence_id: result.evidence_id ?? null,
        collection_scope: result.collection_scope,
        status: result.status,
      },
      { dataDir },
    );

    emit("wicked.agent.task.completed", {
      session_id: sessionId,
      unit_id: u.unit_id,
      phase_status: result.phase_status,
      decision: result.decision ?? null,
      evidence_id: result.evidence_id ?? null,
      status: result.status,
    });

    u.result = result;
  }

  // ── 5. session.completed ──
  const resolvedUnits = units.map((u) => u.result);
  const approvedCount = resolvedUnits.filter((r) => r.status === "done").length;
  const rejectedCount = resolvedUnits.filter((r) => r.status === "rejected").length;
  ledger.updateSession(sessionId, { status: "completed" }, { dataDir });
  emit("wicked.agent.session.completed", {
    session_id: sessionId,
    units: resolvedUnits.length,
    approved: approvedCount,
    rejected: rejectedCount,
  });

  return {
    session_id: sessionId,
    workflow_id: workflowId,
    entity_mode: mode,
    rigor,
    availability: { governance: govStatus, orchestration: orchStatus, council: councilStatus },
    collection_scope: collectionScope,
    vault_scope: vaultScope,
    units: resolvedUnits,
    approved: approvedCount,
    rejected: rejectedCount,
    events,
    status: "completed",
  };
}

export default { runSession, planUnits };
