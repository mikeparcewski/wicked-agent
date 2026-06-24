// wicked-agent — per-CLI injection: FORCING RIGOR on a real wrapped CLI (ARCHITECTURE §5; ADR-0003).
//
// This is the "force rigor" path the session loop's stub stands in for. `launchWrapped`
// runs a wrapped agent CLI as a REAL subprocess to perform a unit's task, and constructs
// its environment so the governance gate is UNAVOIDABLE:
//
//   (b) governance gate — the only piece whose MECHANISM degrades (ADR-0003 step 2/3):
//       · pretool-hook  (capable CLI): wicked-agent writes a PreToolUse hook the CLI must
//         consult before each tool-call. The hook runs governance's deterministic
//         `evaluate` over the proposed tool-call; a `deny` exits non-zero, the CLI aborts
//         the call, and the destructive effect NEVER happens. Enforcement BEFORE the action.
//       · post-hoc      (incapable CLI): the CLI runs free, surfaces its tool-calls, and
//         wicked-agent runs the SAME `evaluate` over each AFTER the fact. A deny rejects the
//         unit and the effect is rolled back. The gate still fires — just later, weaker.
//   (c) phase brackets  — driven by the session loop via the orchestration client (external
//       to the CLI; always available). launchWrapped is given the already-open phase.
//   (d) outputs → collection — the CLI's artifact is captured into the unit's collection scope.
//   (e) evidence → vault — the run's conformance claim is recorded; we return its evidence_id.
//
// THE INVARIANT (ADR-0003): the gate fires on EVERY launch. `blocked:true` means a tool-call
// was denied — pretool aborted it before it ran, or post-hoc rejected the unit after. Either
// way the forbidden effect is not allowed to stand.
//
// Reuse, don't re-invent: the decision engine is governance's (`governanceClient.evaluate`),
// evidence is vault's (`governanceClient.conform` → EvidencePort), capture is the collection
// client's. wicked-agent owns only the GLUE that wires them onto a third-party subprocess.

import { spawnSync } from "node:child_process";
import {
  mkdtempSync,
  mkdirSync,
  writeFileSync,
  readFileSync,
  rmSync,
  existsSync,
  readdirSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));

// Files/dirs wicked-agent INJECTS into the CLI's sandbox — preserved on a rollback so we only
// quarantine the CLI's OWN effects, never the harness's task order or governance hook.
const HARNESS_OWNED = new Set(["TASK.txt", ".wicked-agent-hook"]);

// The pretool hook is a tiny node script wicked-agent drops into the CLI's environment.
// It is the concrete form of "wire `wicked-governance-call evaluate` as the CLI's pre-tool
// hook" (ADR-0003 step 2): the wrapped CLI calls it with its PROPOSED tool-call on stdin
// BEFORE acting; exit 0 ⇒ allow (obligations on stdout), exit 2 ⇒ DENY (the CLI must abort).
// It imports governance's pure decide/select directly — no model, reproducible — exactly the
// per-tool-call decision the in-process governance client uses, but enforced across the real
// subprocess boundary the CLI lives behind.
const HOOK_SOURCE = `#!/usr/bin/env node
// wicked-agent PreToolUse hook (generated). Reads a proposed tool-call as JSON on stdin,
// asks governance, and gates the CLI: exit 0 = allow, exit 2 = DENY (CLI must not proceed).
import { readFileSync } from "node:fs";
import { select } from "GOV_LIB/select.mjs";
import { decide } from "GOV_LIB/decide.mjs";

function readStdin() {
  // fd 0 is the CLI-piped proposed tool-call. Read synchronously; empty on no stdin.
  try { return readFileSync(0, "utf8"); } catch { return ""; }
}

const raw = readStdin();
let call;
try { call = JSON.parse(raw || "{}"); } catch { call = { raw }; }

const phase = process.env.WICKED_GOV_PHASE || "unit";
const scope = process.env.WICKED_GOV_SCOPE || "wicked-agent";
const dataDir = process.env.WICKED_GOV_DATADIR || undefined;

// The governance context for THIS tool-call. The tool + command/args are the work the
// CLI proposes; a deny policy's trigger.contains matches over JSON.stringify(context).
const context = {
  phase,
  scope,
  tool: call.tool,
  command: call.command,
  path: call.path,
  content: call.content,
  args: call.args,
  // 'work' mirrors the session loop's context key so the SAME policies fire either way.
  work: call.command || call.content || call.tool || "",
};

let claim;
try {
  const selected = select({ scope, phase, context, dataDir });
  claim = decide(selected, { ...context, phase }, { scope });
} catch (e) {
  // Fail CLOSED: if governance can't decide, the gate denies (rigor is unavoidable).
  process.stdout.write(JSON.stringify({ decision: "deny", reason: "evaluate failed: " + e.message }));
  process.exit(2);
}

process.stdout.write(JSON.stringify({ decision: claim.decision, obligations: claim.obligations, claim_id: claim.claim_id }));
// deny dominates -> block the tool-call (the CLI aborts on a non-zero exit).
process.exit(claim.decision === "deny" || claim.decision === "reject" ? 2 : 0);
`;

/**
 * Materialize the generated PreToolUse hook for a session, with GOV_LIB resolved to the
 * governance client's own import target so the hook decides EXACTLY like the in-process
 * client. Returns the hook path (executable).
 */
function writeHook(dir, govLibUrl) {
  mkdirSync(dir, { recursive: true });
  const hookPath = join(dir, "pretool-governance-hook.mjs");
  // govLibUrl is a file:// URL or absolute path to wicked-governance/lib.
  const src = HOOK_SOURCE.replaceAll("GOV_LIB", govLibUrl);
  writeFileSync(hookPath, src, { encoding: "utf8", mode: 0o755 });
  return hookPath;
}

/**
 * Resolve the governance lib URL the hook should import. Mirrors the governance client's
 * own resolution (../../wicked-governance/lib relative to lib/clients), so the subprocess
 * hook and the in-process client share one decision engine (ADR-0003 falsifier: evaluate
 * is reused UNCHANGED).
 */
function defaultGovLibUrl() {
  // lib/inject.mjs -> ../../wicked-governance/lib  (sibling, read-only)
  const p = join(__dirname, "..", "..", "wicked-governance", "lib");
  // Use a file:// URL so dynamic import works regardless of platform.
  return "file://" + p.replace(/\\/g, "/");
}

/**
 * Launch a wrapped CLI as a real subprocess to perform a unit's task, GOVERNED + GATED +
 * EVIDENCED. The mechanism of the gate depends on the CLI's capability (pretool vs post-hoc),
 * but the gate ALWAYS fires.
 *
 * The wrapped CLI contract (so this works with any real CLI, and with the E2E's fake-but-real
 * one): the CLI is invoked as `cli.command [cli.args...] <taskFile>`. It reads the task file,
 * performs its real action, and — for governance to evaluate its tool-calls — surfaces them as
 * JSON lines on stdout, each: {"tool_call": {"tool","command","path","content",...}}.
 *
 *   · pretool-hook CLIs additionally consult $WICKED_PRETOOL_HOOK with the proposed tool-call
 *     on stdin BEFORE acting, and abort the action if it exits non-zero.
 *   · post-hoc CLIs just run and emit their tool_call lines; wicked-agent evaluates them after.
 *
 * @param {object} cli  the wrapped CLI descriptor:
 *   - command   : string   the executable (e.g. "node", "/path/to/agent")
 *   - args      : string[] leading args before the task file
 *   - mode      : "pretool-hook" | "post-hoc"  governance_mode (capability) — default "pretool-hook"
 *   - launchable: true (a real launchable CLI; the session loop branches on this)
 * @param {object} unit  the unit of work: { unit_id, description, ... }
 * @param {object} wiring
 *   - scope            : string  the collection scope to capture into.
 *   - phaseClient      : the orchestration client (phase brackets are driven by the caller).
 *   - phaseName        : string  the governance `phase` for this unit (policy applies_to target).
 *   - vaultScope       : string  the vault/governance scope for evidence.
 *   - governanceClient : the governance client (evaluate + conform).
 *   - collectionClient : the collection client (capture).
 *   - vault            : { vaultCwd?: string }  vault root (test isolation).
 *   - govDataDir       : string  governance policy dir.
 *   - dataDir          : string  wicked-agent ledger/collection dir.
 *   - workdir          : string  where the CLI performs its real work (defaults to a temp dir).
 *   - env              : object  extra env for the subprocess.
 *   - govLibUrl        : string  override the governance lib import URL (tests).
 * @returns {Promise<{ tool_calls: object[], blocked: boolean, blocked_reason?: string,
 *                      decision: string, artifact: object|null, artifact_path: string|null,
 *                      evidence_id: string|null, claim: object|null, mode: string,
 *                      gate_timing: "pretool"|"post-hoc", stdout: string, exit_code: number }>}
 */
export async function launchWrapped(cli, unit, wiring = {}) {
  const {
    scope,
    phaseName = "unit",
    vaultScope = scope || "wicked-agent",
    governanceClient,
    collectionClient,
    vault = {},
    govDataDir,
    dataDir,
    env: extraEnv = {},
    govLibUrl = defaultGovLibUrl(),
  } = wiring;

  if (!cli || typeof cli.command !== "string") {
    throw new Error("launchWrapped requires cli.command (the wrapped CLI executable)");
  }
  if (!governanceClient) throw new Error("launchWrapped requires a governanceClient");

  const mode = cli.mode === "post-hoc" ? "post-hoc" : "pretool-hook";
  const gateTiming = mode === "pretool-hook" ? "pretool" : "post-hoc";

  // A real working dir for the CLI's real action. Caller may pin it (so artifacts are
  // inspectable); otherwise an isolated temp dir.
  const ownWorkdir = !wiring.workdir;
  const workdir = wiring.workdir || mkdtempSync(join(tmpdir(), "wicked-agent-cli-"));
  mkdirSync(workdir, { recursive: true });

  // Drop the TASK the CLI reads. This is the unit's real work order.
  const taskFile = join(workdir, "TASK.txt");
  writeFileSync(taskFile, String(unit?.description ?? ""), "utf8");

  // Wire the PreToolUse hook (b). Always materialized; only CONSULTED by pretool-hook CLIs.
  const hookDir = join(workdir, ".wicked-agent-hook");
  const hookPath = writeHook(hookDir, govLibUrl);

  const childEnv = {
    ...process.env,
    // The hook consults these to build the SAME governance context the loop would.
    WICKED_GOV_PHASE: phaseName,
    WICKED_GOV_SCOPE: vaultScope,
    ...(govDataDir ? { WICKED_GOV_DATADIR: govDataDir } : {}),
    // The CLI is TOLD where the hook is + whether it must consult it pre-tool.
    WICKED_PRETOOL_HOOK: mode === "pretool-hook" ? hookPath : "",
    WICKED_GOV_MODE: mode,
    WICKED_TASK_FILE: taskFile,
    WICKED_WORKDIR: workdir,
    ...extraEnv,
  };

  // ── Run the real subprocess. ──
  const args = [...(Array.isArray(cli.args) ? cli.args : []), taskFile];
  const proc = spawnSync(cli.command, args, {
    cwd: workdir,
    encoding: "utf8",
    env: childEnv,
    timeout: wiring.timeout ?? 60000,
  });
  const stdout = proc.stdout || "";
  const stderr = proc.stderr || "";
  const exitCode = proc.status ?? -1;

  // ── Parse the tool-calls the CLI surfaced (one JSON object per line with a tool_call key). ──
  const toolCalls = [];
  for (const line of stdout.split(/\r?\n/)) {
    const s = line.trim();
    if (!s) continue;
    try {
      const obj = JSON.parse(s);
      if (obj && obj.tool_call && typeof obj.tool_call === "object") toolCalls.push(obj.tool_call);
    } catch {
      /* non-JSON line (e.g. human log) — ignore */
    }
  }

  // ── Determine the gate outcome. ──
  let blocked = false;
  let blockedReason;
  let decision = "allow";
  let gatingClaim = null;

  if (mode === "pretool-hook") {
    // PRETOOL: the hook already gated each tool-call BEFORE it ran. A denied call makes the
    // CLI abort with a non-zero exit and (by contract) NOT perform the effect. We still run
    // governance over the proposed calls to produce the authoritative claim + decision.
    for (const call of toolCalls) {
      const context = toolCallContext(call, phaseName, vaultScope);
      const claim = await governanceClient.evaluate(context, { dataDir: govDataDir });
      if (claim.decision === "deny" || claim.decision === "reject") {
        blocked = true;
        blockedReason = `pretool hook denied tool-call (${call.tool || call.command})`;
        decision = claim.decision;
        gatingClaim = claim;
        break;
      }
      // Keep the last allow/allow_with_conditions claim as the evidence for the run.
      decision = claim.decision;
      gatingClaim = claim;
    }
    // Corroborate with the actual subprocess: a pretool-denied run aborts non-zero.
    if (blocked && exitCode === 0) {
      // The CLI claimed to honor the hook but exited 0 after a deny — treat as blocked anyway
      // (the gate's verdict wins; we do not let the effect stand). Surfaced honestly.
      blockedReason += " (CLI exited 0 despite deny; effect rejected by harness)";
    }
  } else {
    // POST-HOC: the CLI already ran. Evaluate each surfaced tool-call AFTER the fact; a deny
    // rejects the unit (and we roll back the effect below). The gate fires — later, weaker.
    for (const call of toolCalls) {
      const context = toolCallContext(call, phaseName, vaultScope);
      const claim = await governanceClient.evaluate(context, { dataDir: govDataDir });
      decision = claim.decision;
      gatingClaim = claim;
      if (claim.decision === "deny" || claim.decision === "reject") {
        blocked = true;
        blockedReason = `post-hoc evaluate denied tool-call (${call.tool || call.command}) after it ran`;
        break;
      }
    }
  }

  // ── (d) capture the artifact + roll back forbidden effects. ──
  // Resolve the CLI's real output artifact (the file it wrote, if any). The CLI declares its
  // primary output path via a tool_call with a `path`; we read it back as the artifact.
  let artifactPath = null;
  for (const call of toolCalls) {
    if (call && typeof call.path === "string" && call.path) {
      artifactPath = resolveOutPath(call.path, workdir);
      break;
    }
  }

  let artifact = null;
  if (!blocked) {
    // Approved: capture the real artifact into the collection scope (d).
    const produced = artifactPath && existsSync(artifactPath) ? safeRead(artifactPath) : null;
    artifact = {
      unit_id: unit?.unit_id,
      description: unit?.description,
      assigned_cli: cli.id || cli.command,
      tool_calls: toolCalls,
      artifact_path: artifactPath,
      output: produced,
      exit_code: exitCode,
    };
    if (collectionClient && scope && unit?.unit_id) {
      collectionClient.write(scope, unit.unit_id, artifact, { dataDir });
    }
  } else {
    // Blocked: the forbidden effect must NOT stand. The wrapped CLI runs SANDBOXED in a workdir
    // wicked-agent owns; on a block we quarantine the CLI's on-disk effects within that dir —
    // the bounded blast radius (ADR-0003 §9.3, falsifier on post-hoc weakness). For pretool the
    // hook aborted before the write so there is usually nothing to undo; for post-hoc the effect
    // already ran (the command/file exists) and is rolled back here.
    rollbackWorkdir(workdir, artifactPath);
    // Nothing is captured to the collection for a blocked unit.
  }

  // ── (e) record evidence of the run via the governance client (→ vault). ──
  // On approval we record the conformance claim (real evidence_id). On a block we do NOT
  // record an approval artifact (a denied unit records no evidence — mirrors the loop).
  let evidenceId = null;
  if (!blocked && gatingClaim) {
    const ev = await governanceClient.conform(gatingClaim, { vaultCwd: vault.vaultCwd });
    evidenceId = ev.evidence_id ?? null;
  }

  // Clean up only the dir WE created (a caller-pinned workdir is theirs to inspect/clean).
  if (ownWorkdir) {
    try {
      rmSync(workdir, { recursive: true, force: true });
    } catch {
      /* best-effort */
    }
  }

  return {
    tool_calls: toolCalls,
    blocked,
    blocked_reason: blockedReason,
    decision,
    artifact,
    artifact_path: artifactPath,
    evidence_id: evidenceId,
    claim: gatingClaim,
    mode,
    gate_timing: gateTiming,
    stdout,
    stderr,
    exit_code: exitCode,
  };
}

/** Build the governance context for a single proposed tool-call (matches the hook). */
function toolCallContext(call, phaseName, scope) {
  return {
    phase: phaseName,
    scope,
    tool: call.tool,
    command: call.command,
    path: call.path,
    content: call.content,
    args: call.args,
    work: call.command || call.content || call.tool || "",
  };
}

/** Resolve a CLI-declared output path against the workdir (absolute paths pass through). */
function resolveOutPath(p, workdir) {
  if (!p) return null;
  return p.startsWith("/") || /^[A-Za-z]:[\\/]/.test(p) ? p : join(workdir, p);
}

/** Read a file as text, returning null on any error (never throws into the loop). */
function safeRead(path) {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
}

/**
 * Roll back the CLI's on-disk effects within the sandbox workdir after a deny (the bounded
 * blast radius, ADR-0003 §9.3). Removes the declared artifact AND any other entry the CLI
 * produced in the workdir, preserving only the harness's injected files (TASK.txt, the hook
 * dir). A Bash side-effect (e.g. a file written by a denied command) is undone even though the
 * harness never knew its path. Best-effort; never throws into the loop.
 */
function rollbackWorkdir(workdir, artifactPath) {
  if (artifactPath && existsSync(artifactPath)) {
    try {
      rmSync(artifactPath, { force: true });
    } catch {
      /* best-effort */
    }
  }
  if (!workdir || !existsSync(workdir)) return;
  let entries = [];
  try {
    entries = readdirSync(workdir);
  } catch {
    return;
  }
  for (const name of entries) {
    if (HARNESS_OWNED.has(name)) continue; // never remove the task order or the governance hook
    try {
      rmSync(join(workdir, name), { recursive: true, force: true });
    } catch {
      /* best-effort quarantine */
    }
  }
}

export default { launchWrapped };
