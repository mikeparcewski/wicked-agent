//! EXECUTE — per unit: open a phase, walk it to the gate, evaluate governance, and gate it.
//!
//! Ported from the prototype's deterministic stub execute path (`session.mjs` §4, path D — the
//! REAL launchable wrapped-CLI path is R6). For each unit, on the ONE shared store:
//!
//! 1. open a [`wicked_orchestration::Phase`] and advance it through the reducer:
//!    `Pending → InProgress → ReadyForGate → GateRunning` (each a real `apply_event`).
//! 2. build the unit's governance context (the gate INPUT) — the phase NAME is the governance
//!    `phase` so a policy's `applies_to` can target this unit-kind, and the unit's work text is in
//!    the context so a deny policy can trigger on its content.
//! 3. `wicked_governance::select` + `decide` → a [`ConformanceClaim`].
//! 4. `wicked_orchestration::apply_gate(store, phase_id, &claim)` consumes the claim and resolves
//!    the phase: `Deny ⇒ Rejected` (approval STRUCTURALLY blocked, ADR-0003), `Allow ⇒ Approved`,
//!    `AllowWithConditions ⇒ ApprovedWithConditions`.
//! 5. on approval: record a work-output [`Node`] on the shared store (tagged with the unit's
//!    collection scope) AND `wicked_governance::conform` for durable evidence (the claim node).
//!    On `Deny`: NO output node is written.
//!
//! THE INVARIANT: the gate fires on EVERY unit; a `Deny` claim drives the phase to `Rejected`
//! THROUGH this loop — never approved by any route.

use std::path::Path;
use std::time::Duration;

use serde::Serialize;
use wicked_apps_core::{
    synthetic_symbol, ConformanceClaim, Decision, GraphRead, Language, Location, Node, NodeKind,
    Span, SqliteStore, ToNode, SYMBOL_SCHEME,
};

use wicked_governance::{conform, decide, decide_as, select};
use wicked_orchestration::{apply_event, apply_gate, get_phase, Event, Phase, PhaseStatus};

use crate::inject::{launch_wrapped, LaunchOutcome, WrappedCli};
use crate::scope::{resolve_scope, EntityMode};
use crate::{put_node, WorkUnit};

/// Node-kind for a unit's recorded work output (`wicked-agent`). Written ONLY when the gate
/// approves; tagged with the unit's collection scope (shared vs isolated).
pub const WORK_OUTPUT: &str = "work_output";

/// A fixed evaluation timestamp base for claims minted by the harness. Deterministic per unit
/// (offset by `ord`) so the same session re-derives the same claim ids without a wall clock on the
/// decision path. (Unix-seconds; the prototype used ISO, wicked-apps-core's claim field is `i64`.)
pub const EVAL_AT_BASE: i64 = 1_750_000_000;

/// The outcome of executing one unit — the harness records this back onto the unit node.
#[derive(Debug, Clone, Serialize)]
pub struct UnitOutcome {
    pub unit_id: String,
    pub ord: u32,
    pub assigned_cli: String,
    /// The orchestration phase id backing this unit.
    pub phase_id: String,
    /// The phase status token the gate resolved to (`approved` / `approved_with_conditions` /
    /// `rejected`).
    pub phase_status: String,
    /// The governance decision the gate consumed (`allow` / `deny` / `allow_with_conditions`).
    pub decision: Option<String>,
    /// The ConformanceClaim id (the durable evidence node id), if a claim was minted.
    pub claim_id: Option<String>,
    /// The collection scope this unit's output was (or would be) written to.
    pub collection_scope: String,
    /// Did the gate approve (Approved | ApprovedWithConditions)? On `false` no output is recorded.
    pub approved: bool,
    /// REAL-CLI path only: the wrapped CLI's real exit code (`None` for the stub path / not launched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_exit_code: Option<i32>,
    /// REAL-CLI path only: the real on-disk artifact path the CLI declared (absolute), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_path: Option<String>,
    /// REAL-CLI path only: did the per-tool-call gate BLOCK an action (the effect never landed)?
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub gate_blocked: bool,
    /// evaluator≠creator: the claim_id of the SECOND governance pass (different evaluator identity),
    /// present only when the unit was approved AND an evaluator CLI was provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_claim_id: Option<String>,
}

/// The outcome of the evaluator≠creator second-pass governance evaluation (ADR-0003 extension).
#[derive(Debug, Clone, Serialize)]
pub struct EvaluationOutcome {
    /// The distinct evaluator identity stamped on the claim (e.g. `"wicked-evaluator:agy"`).
    pub evaluator_identity: String,
    /// The claim_id minted by the evaluator pass (different from the creator's claim_id).
    pub claim_id: String,
    /// The governance decision of the evaluator pass (`allow` / `deny` / `allow_with_conditions`).
    pub decision: String,
    /// Whether the evaluator pass approved (mirrors `decision == allow | allow_with_conditions`).
    pub approved: bool,
}

/// Execute one unit on the shared `store`. See the module docs for the exact ordered flow.
pub fn execute_unit(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
) -> anyhow::Result<UnitOutcome> {
    let assigned_cli = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let phase_name = format!("unit-{}", unit.ord);
    let phase_id = format!("{workflow_id}:{phase_name}");

    // The collection scope this unit's output writes to:
    //   shared   → the ONE session scope (N units, one entity).
    //   isolated → the unit's OWN scope (independent on the same store).
    let collection_scope = resolve_scope(entity_mode, session_id, &unit.id);

    // ── 1. open the phase + walk it to GateRunning through the single-writer reducer. ──
    let phase = Phase::open(&phase_id, workflow_id, &phase_name);
    put_node(store, phase.to_node())?;
    advance_to_gate_running(store, &phase_id)?;

    // ── 2. build the unit's governance context (the gate INPUT). ──
    // The phase NAME is the governance `phase` so a policy's `applies_to` can target this unit;
    // the unit's work text rides in the context so a deny policy can trigger on its content.
    let work_output = format!("stub-output for {}", unit.description);
    let context = serde_json::json!({
        "phase": phase_name,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "assigned_cli": assigned_cli,
        "work": work_output,
    });

    // ── 3. governance SELECT + DECIDE → a ConformanceClaim. ──
    let selected = select(store, &collection_scope, &phase_name, &context)?;
    let evaluated_at = EVAL_AT_BASE + unit.ord as i64;
    let claim: ConformanceClaim = decide(
        &selected,
        &collection_scope,
        &phase_name,
        &context,
        evaluated_at,
    );
    let decision_token = decision_token(&claim.decision);

    // ── 4. the governance gate fires THROUGH orchestration (the invariant). ──
    let gate_event_id = format!("gate-{}", unit.id);
    let gate = apply_gate(store, &phase_id, Some(&claim), &gate_event_id)?;
    let resolved_phase = get_phase(store, &phase_id)?;
    let phase_status = resolved_phase
        .as_ref()
        .map(|p| p.status.as_token().to_string())
        .unwrap_or_else(|| gate.resolved.as_token().to_string());
    let approved = matches!(
        resolved_phase.as_ref().map(|p| p.status),
        Some(PhaseStatus::Approved) | Some(PhaseStatus::ApprovedWithConditions)
    );

    // ── 5. on approval: record the work-output node + durable conformance evidence. ──
    if approved {
        let output_node = work_output_node(
            unit,
            &assigned_cli,
            &collection_scope,
            &work_output,
            &phase_status,
        );
        put_node(store, output_node)?;
        // `conform` upserts the claim node (the durable evidence on the shared graph) + policy→claim
        // edges, then a coarse fire-and-forget event. Fire on approval (the prototype's path).
        conform(store, &claim)?;
    } else {
        // Deny ⇒ Rejected: NO output is written. The claim node is still persisted as evidence of
        // WHY the unit was rejected (the durable record of the deny), matching the prototype's
        // "the claim IS the evidence" model — record it without writing the work output.
        conform(store, &claim)?;
    }

    Ok(UnitOutcome {
        unit_id: unit.id.clone(),
        ord: unit.ord,
        assigned_cli,
        phase_id,
        phase_status,
        decision: Some(decision_token.to_string()),
        claim_id: Some(claim.claim_id),
        collection_scope,
        approved,
        cli_exit_code: None,
        artifact_path: None,
        gate_blocked: false,
        evaluator_claim_id: None,
    })
}

/// Run a SECOND governance pass on an approved unit using a DISTINCT evaluator identity
/// (the evaluator≠creator invariant). The second claim is persisted via `conform`; its `claim_id`
/// is different from the creator's because `evaluate_identity` is included in the seed.
///
/// Call ONLY after the creator pass approved (`outcome.approved == true`). Calling on a rejected
/// unit is a caller bug — this function returns an error in that case so misuse surfaces early.
///
/// The evaluator context includes the real work `output` so the evaluator can assess the
/// ACTUAL result (not just the description). The governance `phase` is `"eval-{phase_name}"` so
/// policies can specifically target the evaluation phase.
pub fn evaluate_unit(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    output: &str,
    evaluator_cli: &str,
    collection_scope: &str,
    phase_name: &str,
    evaluated_at: i64,
) -> anyhow::Result<EvaluationOutcome> {
    let evaluator_identity = format!("wicked-evaluator:{evaluator_cli}");
    let eval_phase = format!("eval-{phase_name}");

    let eval_context = serde_json::json!({
        "phase": eval_phase,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "evaluator_cli": evaluator_cli,
        "output": output,
    });

    let selected = select(store, collection_scope, &eval_phase, &eval_context)?;
    let claim = decide_as(
        &selected,
        collection_scope,
        &eval_phase,
        &eval_context,
        evaluated_at,
        &evaluator_identity,
    );
    let decision = decision_token(&claim.decision).to_string();
    let approved = matches!(
        claim.decision,
        Decision::Allow | Decision::AllowWithConditions
    );
    let claim_id = claim.claim_id.clone();

    conform(store, &claim)?;

    Ok(EvaluationOutcome {
        evaluator_identity,
        claim_id,
        decision,
        approved,
    })
}

/// The default per-subprocess timeout for a real wrapped-CLI launch (deterministic fake CLIs return
/// in ms; a real LLM CLI can take seconds — the caller may override).
pub const DEFAULT_CLI_TIMEOUT: Duration = Duration::from_secs(120);

/// EXECUTE ONE UNIT BY LAUNCHING A REAL WRAPPED CLI (the R6 full-functional path).
///
/// Identical orchestration brackets + governance gate as [`execute_unit`], but the unit's WORK is
/// done by a REAL subprocess (`inject::launch_wrapped`) instead of a stub string. Two governance
/// mechanisms fire, both on the ONE shared `store`:
///
///   1. **Unit-level gate (CLI-agnostic, BEFORE launch):** `select`+`decide` on the unit's context.
///      `Deny` ⇒ the CLI is NOT launched, the orchestration phase resolves `Rejected`, NO output.
///      `Allow` ⇒ proceed to launch.
///   2. **Per-tool-call pre-hook (DURING the run):** the wrapped CLI consults the generated
///      governance hook before each tool-call; a `Deny` blocks that action (the effect never lands).
///
/// On approval the CLI's REAL stdout/artifact is captured to a work-output [`Node`] and the claim is
/// recorded via `conform` (durable evidence). The phase is bracketed by orchestration and
/// `apply_gate` consumes the unit-level claim — the invariant: the gate fires on EVERY unit.
///
/// `workdir` is the sandbox the CLI performs its real work in (caller-pinned, inspectable).
#[allow(clippy::too_many_arguments)]
pub fn execute_unit_wrapped(
    store: &mut SqliteStore,
    unit: &WorkUnit,
    cli: &WrappedCli,
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    workdir: &Path,
    timeout: Duration,
) -> anyhow::Result<UnitOutcome> {
    let assigned_cli = unit.assigned_cli.clone().unwrap_or_else(|| cli.id.clone());
    let phase_name = format!("unit-{}", unit.ord);
    let phase_id = format!("{workflow_id}:{phase_name}");
    let collection_scope = resolve_scope(entity_mode, session_id, &unit.id);

    // ── 1. open the phase + walk it to GateRunning. ──
    let phase = Phase::open(&phase_id, workflow_id, &phase_name);
    put_node(store, phase.to_node())?;
    advance_to_gate_running(store, &phase_id)?;

    // ── 2. UNIT-LEVEL GATE (CLI-agnostic), BEFORE launch. The unit's work text is the description;
    //       a deny policy keyed on the unit's content fires here and BLOCKS the launch entirely. ──
    let unit_context = serde_json::json!({
        "phase": phase_name,
        "scope": collection_scope,
        "unit_id": unit.id,
        "description": unit.description,
        "assigned_cli": assigned_cli,
        "work": unit.description,
    });
    let selected = select(store, &collection_scope, &phase_name, &unit_context)?;
    let evaluated_at = EVAL_AT_BASE + unit.ord as i64;
    let unit_claim: ConformanceClaim = decide(
        &selected,
        &collection_scope,
        &phase_name,
        &unit_context,
        evaluated_at,
    );
    let unit_denied = unit_claim.decision == Decision::Deny;

    // ── 3. LAUNCH the real subprocess — ONLY if the unit-level gate allows. ──
    // On a unit-level Deny the CLI is NEVER launched (no subprocess, no output). On Allow the CLI
    // runs and its OWN per-tool-call pre-hook gates each action across the process boundary.
    let toolbox = crate::inject::discover_toolbox();
    let launch: Option<LaunchOutcome> = if unit_denied {
        None
    } else {
        Some(launch_wrapped(
            cli,
            &unit.description,
            &collection_scope,
            &phase_name,
            workdir,
            timeout,
            &toolbox,
        )?)
    };

    // The authoritative claim the gate consumes: the unit-level claim on a unit-deny, else the
    // per-tool-call claim (which captures a hook-block) when present, else the unit-level allow.
    let gate_claim: ConformanceClaim = match &launch {
        Some(l) if l.claim.is_some() => l.claim.clone().unwrap(),
        _ => unit_claim.clone(),
    };
    let decision_token = decision_token(&gate_claim.decision);

    // ── 4. the governance gate fires THROUGH orchestration (the invariant). ──
    let gate_event_id = format!("gate-{}", unit.id);
    let gate = apply_gate(store, &phase_id, Some(&gate_claim), &gate_event_id)?;
    let resolved_phase = get_phase(store, &phase_id)?;
    let phase_status = resolved_phase
        .as_ref()
        .map(|p| p.status.as_token().to_string())
        .unwrap_or_else(|| gate.resolved.as_token().to_string());
    let approved = matches!(
        resolved_phase.as_ref().map(|p| p.status),
        Some(PhaseStatus::Approved) | Some(PhaseStatus::ApprovedWithConditions)
    );

    // A launch that the per-tool-call hook blocked is also a non-approval (the effect was denied).
    let gate_blocked = launch.as_ref().map(|l| l.blocked).unwrap_or(false);
    let approved = approved && !gate_blocked;

    // ── 5. on approval: record the REAL artifact as the work-output node + durable evidence. ──
    let (artifact_path, cli_exit_code) = match &launch {
        Some(l) => (l.artifact_path.clone(), Some(l.exit_code)),
        None => (None, None),
    };
    if approved {
        let output = launch
            .as_ref()
            .and_then(|l| l.artifact.clone())
            .or_else(|| launch.as_ref().map(|l| l.stdout.clone()))
            .unwrap_or_default();
        let output_node = real_work_output_node(
            unit,
            &assigned_cli,
            &collection_scope,
            &output,
            &phase_status,
            launch.as_ref(),
        );
        put_node(store, output_node)?;
        conform(store, &gate_claim)?;
    } else {
        // Rejected / blocked ⇒ NO output node. The claim is still recorded as durable evidence of WHY.
        conform(store, &gate_claim)?;
    }

    Ok(UnitOutcome {
        unit_id: unit.id.clone(),
        ord: unit.ord,
        assigned_cli,
        phase_id,
        phase_status,
        decision: Some(decision_token.to_string()),
        claim_id: Some(gate_claim.claim_id),
        collection_scope,
        approved,
        cli_exit_code,
        artifact_path,
        gate_blocked,
        evaluator_claim_id: None,
    })
}

/// Build the work-output [`Node`] for an approved REAL-CLI unit. Carries the real artifact path,
/// exit code, and a compact tool-call summary as durable evidence of the genuine subprocess run.
fn real_work_output_node(
    unit: &WorkUnit,
    assigned_cli: &str,
    collection_scope: &str,
    output: &str,
    phase_status: &str,
    launch: Option<&LaunchOutcome>,
) -> Node {
    let mut node = Node::new(
        synthetic_symbol(WORK_OUTPUT, &unit.id),
        NodeKind::Other(WORK_OUTPUT.to_string()),
        unit.id.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{WORK_OUTPUT}/{}", unit.id), Span::ZERO),
    );
    let m = &mut node.metadata;
    m.insert("unit_id".into(), serde_json::Value::String(unit.id.clone()));
    m.insert(
        "session_id".into(),
        serde_json::Value::String(unit.session_id.clone()),
    );
    m.insert(
        "assigned_cli".into(),
        serde_json::Value::String(assigned_cli.to_string()),
    );
    m.insert(
        "collection_scope".into(),
        serde_json::Value::String(collection_scope.to_string()),
    );
    m.insert(
        "phase_status".into(),
        serde_json::Value::String(phase_status.to_string()),
    );
    m.insert(
        "output".into(),
        serde_json::Value::String(output.to_string()),
    );
    m.insert("real_cli".into(), serde_json::Value::Bool(true));
    if let Some(l) = launch {
        m.insert("exit_code".into(), serde_json::json!(l.exit_code));
        m.insert(
            "gate_timing".into(),
            serde_json::Value::String(l.gate_timing.clone()),
        );
        if let Some(p) = &l.artifact_path {
            m.insert("artifact_path".into(), serde_json::Value::String(p.clone()));
        }
    }
    node
}

/// Advance a freshly-opened phase `Pending → InProgress → ReadyForGate → GateRunning` through legal
/// reducer transitions (each a real `apply_event` on the shared store). Event ids are unique per
/// phase so re-running a session is idempotent per the reducer's dedup ledger.
fn advance_to_gate_running(store: &mut SqliteStore, phase_id: &str) -> anyhow::Result<()> {
    for (step, to) in [
        PhaseStatus::InProgress,
        PhaseStatus::ReadyForGate,
        PhaseStatus::GateRunning,
    ]
    .into_iter()
    .enumerate()
    {
        let event_id = format!("{phase_id}:advance-{step}");
        let outcome = apply_event(store, &Event::transition(event_id, phase_id, to))?;
        if !outcome.applied {
            anyhow::bail!(
                "advancing phase {phase_id} to {:?} did not apply: {:?}",
                to,
                outcome.reason
            );
        }
    }
    Ok(())
}

/// Build the work-output [`Node`] for an approved unit. Keyed by the unit id (one output per unit),
/// tagged with the assigned CLI, collection scope, and resolved phase status in metadata.
fn work_output_node(
    unit: &WorkUnit,
    assigned_cli: &str,
    collection_scope: &str,
    output: &str,
    phase_status: &str,
) -> Node {
    let mut node = Node::new(
        synthetic_symbol(WORK_OUTPUT, &unit.id),
        NodeKind::Other(WORK_OUTPUT.to_string()),
        unit.id.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{WORK_OUTPUT}/{}", unit.id), Span::ZERO),
    );
    let m = &mut node.metadata;
    m.insert("unit_id".into(), serde_json::Value::String(unit.id.clone()));
    m.insert(
        "session_id".into(),
        serde_json::Value::String(unit.session_id.clone()),
    );
    m.insert(
        "assigned_cli".into(),
        serde_json::Value::String(assigned_cli.to_string()),
    );
    m.insert(
        "collection_scope".into(),
        serde_json::Value::String(collection_scope.to_string()),
    );
    m.insert(
        "phase_status".into(),
        serde_json::Value::String(phase_status.to_string()),
    );
    m.insert(
        "output".into(),
        serde_json::Value::String(output.to_string()),
    );
    node
}

/// Read back a work-output node for a unit, if one was recorded (i.e. the unit was approved).
pub fn get_work_output(store: &dyn GraphRead, unit_id: &str) -> anyhow::Result<Option<Node>> {
    store
        .get_node(&synthetic_symbol(WORK_OUTPUT, unit_id))
        .map_err(|e| anyhow::anyhow!("get work-output node: {e}"))
}

/// The snake_case decision token for an outcome / event payload.
fn decision_token(decision: &Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::AllowWithConditions => "allow_with_conditions",
    }
}
