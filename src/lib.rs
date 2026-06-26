//! wicked-agent — the in-process harness (R5 capstone) that drives **governance +
//! orchestration + council** on ONE shared wicked-estate store (the collection, finally real).
//!
//! This is the Rust port of the Node prototype (`wicked-agent/lib/session.mjs`, ARCHITECTURE §3:
//! the OWNED interactive flow) onto the wicked-apps spine (`wicked-apps-core`). [`run_session`] DRIVES the
//! three sibling crates through **plan → distribute → execute**, under the estate's rigor:
//!
//! ```text
//!   plan       → decompose the problem into ordered WorkUnits; persist the session +
//!                each unit as estate Nodes on the SHARED store.
//!   distribute → wicked_council (queue → poll a real verdict, in-process) picks the
//!                assigned CLI per unit; the assignment is recorded on the unit node.
//!   execute    → per unit: open a wicked_orchestration Phase, advance to GateRunning,
//!                wicked_governance::select + decide → a ConformanceClaim, then
//!                wicked_orchestration::apply_gate consumes it. Approved ⇒ a work-output
//!                Node is recorded (+ wicked_governance::conform for durable evidence);
//!                Deny ⇒ the phase resolves Rejected and NO output is written.
//! ```
//!
//! ## THE INVARIANT (ADR-0003)
//! The governance gate fires on EVERY unit. A `Deny` claim STRUCTURALLY blocks that unit's
//! approval (orchestration's persisted `gate_decision` veto) — the harness never approves a denied
//! unit by any route. The integration test proves a denied unit's phase resolves to `Rejected`
//! THROUGH this loop, in-process, on the shared store.
//!
//! ## ONE shared store (the integration shape)
//! `SqliteStore` owns a non-cloneable `rusqlite::Connection`; an in-memory store is private to its
//! one handle. So the harness holds exactly ONE [`SqliteStore`] and drives governance +
//! orchestration directly against it (their `&mut dyn GraphStore` / `&mut S` API). The session
//! node, every work-unit node, the ConformanceClaim node (via `conform`), every phase node (via the
//! reducer/gate), and every work-output node ALL land on that one store.
//!
//! The council is convened **in-process** and its decision flows back into the harness, which
//! records the assigned CLI onto the shared work-unit node. The council's own task/verdict/ranking
//! persistence (its internal `EstateHandle` ledger — exercised by the council's own E2E) is the
//! council's concern; the agent's shared-collection contract is the session/unit/claim/phase/output
//! nodes listed above. (Real agent CLIs — claude/agy/pi — are R6; here the council is driven over
//! deterministic fake-CLI seats, exactly like the council's own tests.)

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
    ToNode, AGENT_SESSION, SYMBOL_SCHEME, WORK_UNIT,
};

pub mod distribute;
pub mod execute;
pub mod inject;
pub mod plan;
pub mod resume;
pub mod scope;

pub use distribute::{distribute_units, distribute_units_on, Distribution};
pub use execute::{
    evaluate_unit, execute_unit, execute_unit_wrapped, EvaluationOutcome, UnitOutcome,
    DEFAULT_CLI_TIMEOUT, WORK_OUTPUT,
};
pub use inject::{
    discover_toolbox, launch_wrapped, parse_decisions_file, run_gate_hook, write_mcp_config,
    GovernanceMode, LaunchOutcome, McpServerSpec, WrappedCli,
};
pub use plan::plan_units;
pub use resume::{get_cursor, put_cursor, ResumeCursor, AGENT_RESUME_CURSOR};
pub use scope::{resolve_scope, EntityMode};

// ─────────────────────────────────────────────────────────────────────────────
// Coarse bus events (counts / ids only) — mirrors the wicked-apps-core agent catalog.
// ─────────────────────────────────────────────────────────────────────────────

pub use wicked_apps_core::{
    EV_AGENT_PLAN_CREATED, EV_AGENT_SESSION_COMPLETED, EV_AGENT_SESSION_STARTED,
    EV_AGENT_TASK_COMPLETED, EV_AGENT_WORK_DISTRIBUTED,
};

/// Crate identity smoke.
pub fn health() -> &'static str {
    "wicked-agent"
}

// ─────────────────────────────────────────────────────────────────────────────
// Domain: AgentSession + WorkUnit, projected onto the shared estate store.
// ─────────────────────────────────────────────────────────────────────────────

/// The lifecycle status of an [`AgentSession`] (mirrors the prototype's session status string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Planning,
    Distributing,
    Executing,
    /// The run paused BEFORE a not-yet-done unit, awaiting a human to resume (a [`ResumeCursor`] is
    /// persisted). `wicked-agent resume <session_id>` continues from where it paused.
    AwaitingHuman,
    Completed,
}

/// The human-confirm gate policy for a wrapped session run — decides whether the harness pauses
/// BEFORE executing a unit so a human can confirm (and later `resume`).
///
/// - [`None`](HumanConfirm::None): never pause (the default — byte-for-byte the pre-gate behavior).
/// - [`All`](HumanConfirm::All): pause before EVERY not-yet-done unit.
/// - [`Before`](HumanConfirm::Before): pause before the unit whose `ord` equals the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HumanConfirm {
    #[default]
    None,
    All,
    Before(u32),
}

/// An agent session — the OWNED interactive flow, persisted as `Node(Other(AGENT_SESSION))` on the
/// shared store. Every field is round-tripped through `Node.metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSession {
    /// Stable session id (the node identity).
    pub id: String,
    /// The orchestration workflow id that backs this session.
    pub workflow_id: String,
    /// The free-text problem this session decomposes.
    pub problem: String,
    /// Shared (one collection scope for all units) vs isolated (per-unit scope) — §6 toggle.
    pub entity_mode: EntityMode,
    /// The collection scope under shared mode (`None` under isolated — each unit gets its own).
    pub collection_scope: Option<String>,
    /// The CLI seats convened for this session (council options).
    pub clis: Vec<String>,
    /// Lifecycle status.
    pub status: SessionStatus,
    /// The human-confirm gate policy for this run. `#[serde(default)]` so sessions persisted BEFORE
    /// this field existed still deserialize (defaulting to [`HumanConfirm::None`]).
    #[serde(default)]
    pub human_confirm: HumanConfirm,
}

impl ToNode for AgentSession {
    fn node_kind() -> &'static str {
        AGENT_SESSION
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(AGENT_SESSION, &self.id),
            NodeKind::Other(AGENT_SESSION.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{AGENT_SESSION}/{}", self.id), Span::ZERO),
        );
        // The whole struct round-trips through one metadata object (lossless, no per-field plumbing).
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("AgentSession serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for AgentSession {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == AGENT_SESSION => {}
            other => anyhow::bail!("expected NodeKind::Other({AGENT_SESSION:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid AgentSession: {e}", node.name))
    }
}

/// A unit of distributed agent work, persisted as `Node(Other(WORK_UNIT))` on the shared store.
/// The plan creates it `Pending`; distribute records the assignment; execute records the outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUnit {
    /// Stable unit id (the node identity), e.g. `<session>:u1`.
    pub id: String,
    /// The owning session id.
    pub session_id: String,
    /// 1-based order in the plan.
    pub ord: u32,
    /// The unit's description (becomes the gate's governance context `work`).
    pub description: String,
    /// The CLI the council assigned (set in distribute; `None` until then).
    #[serde(default)]
    pub assigned_cli: Option<String>,
    /// The council task id whose verdict produced the assignment (provenance; `None` until distribute).
    #[serde(default)]
    pub council_task_ref: Option<String>,
    /// The orchestration phase id that backs this unit (set in execute).
    #[serde(default)]
    pub phase_ref: Option<String>,
    /// The ConformanceClaim id the gate consumed (set in execute).
    #[serde(default)]
    pub conformance_ref: Option<String>,
    /// The phase status token the gate resolved to (set in execute), e.g. `approved` / `rejected`.
    #[serde(default)]
    pub phase_status: Option<String>,
    /// The collection scope this unit's output is written to (shared vs isolated).
    #[serde(default)]
    pub collection_scope: Option<String>,
    /// The final unit status: `pending` → `distributed` → `done` | `rejected`.
    pub status: UnitStatus,
}

/// The lifecycle status of a [`WorkUnit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Pending,
    Distributed,
    Done,
    Rejected,
}

impl WorkUnit {
    /// Build a fresh `Pending` unit for the plan.
    pub fn pending(
        id: impl Into<String>,
        session_id: impl Into<String>,
        ord: u32,
        description: impl Into<String>,
    ) -> Self {
        WorkUnit {
            id: id.into(),
            session_id: session_id.into(),
            ord,
            description: description.into(),
            assigned_cli: None,
            council_task_ref: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            status: UnitStatus::Pending,
        }
    }
}

impl ToNode for WorkUnit {
    fn node_kind() -> &'static str {
        WORK_UNIT
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(WORK_UNIT, &self.id),
            NodeKind::Other(WORK_UNIT.to_string()),
            self.id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(format!("{WORK_UNIT}/{}", self.id), Span::ZERO),
        );
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("WorkUnit serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for WorkUnit {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == WORK_UNIT => {}
            other => anyhow::bail!("expected NodeKind::Other({WORK_UNIT:?}), got {other:?}"),
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid WorkUnit: {e}", node.name))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Store helpers — the single shared-store write/read primitives the harness uses.
// ─────────────────────────────────────────────────────────────────────────────

/// Upsert a node onto the shared store via the batch write path.
pub(crate) fn put_node(store: &mut SqliteStore, node: Node) -> anyhow::Result<()> {
    use wicked_apps_core::GraphWrite;
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Read an [`AgentSession`] back from the shared store by id.
pub fn get_session(
    store: &dyn GraphRead,
    session_id: &str,
) -> anyhow::Result<Option<AgentSession>> {
    match store.get_node(&synthetic_symbol(AGENT_SESSION, session_id))? {
        Some(node) => Ok(Some(AgentSession::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Read a [`WorkUnit`] back from the shared store by id.
pub fn get_work_unit(store: &dyn GraphRead, unit_id: &str) -> anyhow::Result<Option<WorkUnit>> {
    match store.get_node(&synthetic_symbol(WORK_UNIT, unit_id))? {
        Some(node) => Ok(Some(WorkUnit::from_node(&node)?)),
        None => Ok(None),
    }
}

/// Read back every [`WorkUnit`] belonging to `session_id`, ordered by `ord`.
pub fn session_units(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Vec<WorkUnit>> {
    use wicked_estate_core::SymbolQuery;
    let query = SymbolQuery {
        kinds: vec![NodeKind::Other(WORK_UNIT.to_string())],
        ..Default::default()
    };
    let mut units: Vec<WorkUnit> = store
        .find_symbols(&query)?
        .iter()
        .filter_map(|n| WorkUnit::from_node(n).ok())
        .filter(|u| u.session_id == session_id)
        .collect();
    units.sort_by_key(|u| u.ord);
    Ok(units)
}

// ─────────────────────────────────────────────────────────────────────────────
// run_session — the harness flow over ONE shared store.
// ─────────────────────────────────────────────────────────────────────────────

/// The result of a [`run_session`] call (mirrors the prototype's ledger doc + a units summary).
#[derive(Debug, Clone, Serialize)]
pub struct SessionResult {
    pub session_id: String,
    pub workflow_id: String,
    pub entity_mode: EntityMode,
    pub collection_scope: Option<String>,
    /// Per-unit outcomes in plan order.
    pub units: Vec<UnitOutcome>,
    pub approved: usize,
    pub rejected: usize,
    /// `Some(ord)` when the run PAUSED before that unit (a [`ResumeCursor`] is persisted); `None` on
    /// full completion. Only the wrapped run/resume path sets this; `run_session` always reports `None`.
    #[serde(default)]
    pub paused_at: Option<u32>,
}

/// Run a full governed session over ONE shared [`SqliteStore`].
///
/// `clis` is the convened council roster (the `wicked_council::AgenticCli` seats — deterministic
/// fake-CLI scripts here, real CLIs in R6). `problem` is decomposed into ordered units; `entity_mode`
/// is the shared-vs-isolated scope toggle (§6).
///
/// Everything persists on the ONE `store`: the session node, each work-unit node (with its
/// assignment + outcome), the ConformanceClaim node, each phase node, and each approved unit's
/// work-output node.
pub fn run_session(
    store: &mut SqliteStore,
    clis: Vec<wicked_council::AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: Option<&str>,
) -> anyhow::Result<SessionResult> {
    use wicked_apps_core::emit::{emit_event, EmitEvent};

    let session_id = session_id.map(str::to_string).unwrap_or_else(|| {
        format!(
            "sess-{}",
            deterministic_id(&[problem, &clis.len().to_string()])
        )
    });
    let workflow_id = format!("wf-{session_id}");

    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    // Shared: one collection scope for all units. Isolated: per-unit (resolved in execute).
    let collection_scope = match entity_mode {
        EntityMode::Shared => Some(scope::resolve_scope(entity_mode, &session_id, "shared")),
        EntityMode::Isolated => None,
    };

    // ── 1. PLAN — session.started + plan.created; persist session + units on the shared store. ──
    let mut session = AgentSession {
        id: session_id.clone(),
        workflow_id: workflow_id.clone(),
        problem: problem.to_string(),
        entity_mode,
        collection_scope: collection_scope.clone(),
        clis: cli_keys.clone(),
        status: SessionStatus::Planning,
        human_confirm: HumanConfirm::None,
    };
    put_node(store, session.to_node())?;

    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_SESSION_STARTED,
        "wicked-agent",
        "agent.session",
        serde_json::json!({
            "session_id": session_id,
            "workflow_id": workflow_id,
            "entity_mode": entity_mode,
            "clis": cli_keys,
        }),
    ));

    let mut units = plan::plan_units(problem, &session_id);
    for u in &units {
        put_node(store, u.to_node())?;
    }
    session.status = SessionStatus::Distributing;
    put_node(store, session.to_node())?;

    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_PLAN_CREATED,
        "wicked-agent",
        "agent.plan",
        serde_json::json!({
            "session_id": session_id,
            "unit_count": units.len(),
            "unit_ids": units.iter().map(|u| u.id.as_str()).collect::<Vec<_>>(),
        }),
    ));

    // Register the workflow node AFTER planning so the cursor can be ticked per unit in execute.
    // execute_unit manages each phase's lifecycle itself; register_workflow only persists the ordered
    // phase list + cursor so the workflow is queryable without opening any phases.
    let phase_specs: Vec<(String, String)> = units
        .iter()
        .map(|u| {
            let phase_name = format!("unit-{}", u.ord);
            let phase_id = format!("{workflow_id}:{phase_name}");
            (phase_id, u.description.clone())
        })
        .collect();
    wicked_orchestration::register_workflow(store, &workflow_id, problem, &phase_specs)?;

    // ── 2. DISTRIBUTE — the council (in-process) picks the assigned CLI per unit. ──
    let distributions = distribute::distribute_units(&units, &clis, &session_id)?;
    for (u, dist) in units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.council_task_ref = dist.council_task_ref.clone();
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;

        let _ = emit_event(&EmitEvent::new(
            EV_AGENT_WORK_DISTRIBUTED,
            "wicked-agent",
            "agent.work",
            serde_json::json!({
                "session_id": session_id,
                "unit_id": u.id,
                "assigned_cli": dist.assigned_cli,
                "council_degraded": dist.degraded,
            }),
        ));
    }

    // ── 3. EXECUTE — per unit: phase → governance → gate, all on the shared store. ──
    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &mut units {
        let mut outcome = execute::execute_unit(store, u, &workflow_id, entity_mode, &session_id)?;

        // evaluator≠creator: run a second governance pass on approved units using a different CLI
        // as the evaluator identity. Pick the NEXT CLI in the roster (or any other if only one seat).
        if outcome.approved {
            let evaluator_cli = next_cli_in_roster(&outcome.assigned_cli, &cli_keys);
            let eval_at = execute::EVAL_AT_BASE + u.ord as i64 + 1_000_000;
            if let Ok(eval) = execute::evaluate_unit(
                store,
                u,
                &format!("stub-output for {}", u.description),
                &evaluator_cli,
                &outcome.collection_scope,
                &format!("unit-{}", u.ord),
                eval_at,
            ) {
                outcome.evaluator_claim_id = Some(eval.claim_id);
            }
        }

        // Tick the workflow cursor AFTER the unit completes (does not open the next phase —
        // execute_unit manages that in the next loop iteration).
        wicked_orchestration::tick_workflow(store, &workflow_id, outcome.approved)?;

        // Record the unit's outcome back onto its shared-store node.
        u.phase_ref = Some(outcome.phase_id.clone());
        u.conformance_ref = outcome.claim_id.clone();
        u.phase_status = Some(outcome.phase_status.clone());
        u.collection_scope = Some(outcome.collection_scope.clone());
        u.status = if outcome.approved {
            UnitStatus::Done
        } else {
            UnitStatus::Rejected
        };
        put_node(store, u.to_node())?;

        let _ = emit_event(&EmitEvent::new(
            EV_AGENT_TASK_COMPLETED,
            "wicked-agent",
            "agent.task",
            serde_json::json!({
                "session_id": session_id,
                "unit_id": u.id,
                "phase_status": outcome.phase_status,
                "decision": outcome.decision,
                "status": if outcome.approved { "done" } else { "rejected" },
            }),
        ));

        outcomes.push(outcome);
    }

    // ── 4. session.completed — everything persists on the ONE store. ──
    session.status = SessionStatus::Completed;
    put_node(store, session.to_node())?;

    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;

    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_SESSION_COMPLETED,
        "wicked-agent",
        "agent.session",
        serde_json::json!({
            "session_id": session_id,
            "units": outcomes.len(),
            "approved": approved,
            "rejected": rejected,
        }),
    ));

    Ok(SessionResult {
        session_id,
        workflow_id,
        entity_mode,
        collection_scope,
        units: outcomes,
        approved,
        rejected,
        paused_at: None,
    })
}

/// Run a full governed session over ONE shared [`SqliteStore`], LAUNCHING the council-assigned CLI
/// as a REAL subprocess per unit (the R6 full-functional path).
///
/// Identical plan → distribute flow as [`run_session`], but EXECUTE launches the real wrapped CLI
/// (`execute_unit_wrapped`) instead of the stub: a unit-level governance gate fires BEFORE launch
/// (`Deny` ⇒ no subprocess, phase `Rejected`, no output), and the launched CLI's per-tool-call
/// pre-hook gates each action across the process boundary. The assigned CLI key is mapped back to a
/// [`WrappedCli`] by matching the convened `clis` roster (its `binary`/`headless_invocation` give the
/// real command). `sandbox_root` is where each unit's sandbox workdir is created.
///
/// `governance_mode` selects the per-tool-call mechanism for the launched CLIs (pretool-hook vs
/// post-hoc). Everything persists on the ONE `store`: session, units (+assignment/outcome), claims,
/// phases, and each approved unit's REAL work-output node.
///
/// `human_confirm` is the pause-before-unit gate (see [`HumanConfirm`]). When the run pauses, a
/// [`ResumeCursor`] is persisted, the session + workflow are set to `AwaitingHuman`, and the call
/// returns early with `paused_at = Some(ord)`. [`resume_session`] continues from there. The default
/// [`HumanConfirm::None`] never pauses — the run is byte-for-byte the pre-gate behavior.
#[allow(clippy::too_many_arguments)]
pub fn run_session_wrapped(
    store: &mut SqliteStore,
    clis: Vec<wicked_council::AgenticCli>,
    problem: &str,
    entity_mode: EntityMode,
    session_id: Option<&str>,
    governance_mode: inject::GovernanceMode,
    sandbox_root: &std::path::Path,
    timeout: std::time::Duration,
    human_confirm: HumanConfirm,
) -> anyhow::Result<SessionResult> {
    use wicked_apps_core::emit::{emit_event, EmitEvent};

    let session_id = session_id.map(str::to_string).unwrap_or_else(|| {
        format!(
            "sess-{}",
            deterministic_id(&[problem, &clis.len().to_string()])
        )
    });
    let workflow_id = format!("wf-{session_id}");
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let collection_scope = match entity_mode {
        EntityMode::Shared => Some(scope::resolve_scope(entity_mode, &session_id, "shared")),
        EntityMode::Isolated => None,
    };

    // ── 1. PLAN. ──
    let mut session = AgentSession {
        id: session_id.clone(),
        workflow_id: workflow_id.clone(),
        problem: problem.to_string(),
        entity_mode,
        collection_scope: collection_scope.clone(),
        clis: cli_keys.clone(),
        status: SessionStatus::Planning,
        human_confirm,
    };
    put_node(store, session.to_node())?;
    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_SESSION_STARTED,
        "wicked-agent",
        "agent.session",
        serde_json::json!({ "session_id": session_id, "workflow_id": workflow_id, "entity_mode": entity_mode, "clis": cli_keys }),
    ));

    let mut units = plan::plan_units(problem, &session_id);
    for u in &units {
        put_node(store, u.to_node())?;
    }
    session.status = SessionStatus::Distributing;
    put_node(store, session.to_node())?;
    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_PLAN_CREATED,
        "wicked-agent",
        "agent.plan",
        serde_json::json!({ "session_id": session_id, "unit_count": units.len() }),
    ));

    // Register workflow node for the wrapped session (same as stub path).
    let phase_specs_w: Vec<(String, String)> = units
        .iter()
        .map(|u| {
            let phase_name = format!("unit-{}", u.ord);
            let phase_id = format!("{workflow_id}:{phase_name}");
            (phase_id, u.description.clone())
        })
        .collect();
    wicked_orchestration::register_workflow(store, &workflow_id, problem, &phase_specs_w)?;

    // ── 2. DISTRIBUTE — the council (in-process, REAL verdict) picks the assigned CLI per unit. ──
    // The council shares the SAME on-disk file (its task/verdict land alongside the agent's entities,
    // R6) — resolved from WICKED_ESTATE_DB, which the caller exports for the gate-hook child too. The
    // agent's `store` handle is idle during distribution; the council uses its own connection.
    let db_path = std::env::var("WICKED_ESTATE_DB")
        .ok()
        .filter(|p| !p.is_empty() && p != ":memory:");
    let distributions =
        distribute::distribute_units_on(&units, &clis, &session_id, db_path.as_deref())?;
    for (u, dist) in units.iter_mut().zip(distributions.iter()) {
        u.assigned_cli = Some(dist.assigned_cli.clone());
        u.council_task_ref = dist.council_task_ref.clone();
        u.status = UnitStatus::Distributed;
        put_node(store, u.to_node())?;
        let _ = emit_event(&EmitEvent::new(
            EV_AGENT_WORK_DISTRIBUTED,
            "wicked-agent",
            "agent.work",
            serde_json::json!({ "session_id": session_id, "unit_id": u.id, "assigned_cli": dist.assigned_cli, "council_degraded": dist.degraded }),
        ));
    }

    // ── 3. EXECUTE — per unit: launch the REAL assigned CLI under the governance gate. ──
    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &mut units {
        // HUMAN-CONFIRM GATE: pause BEFORE executing this unit if the policy says so — but never
        // pause on a unit already resolved (resume safety; only relevant when this is a re-run).
        let already_resolved = matches!(u.status, UnitStatus::Done | UnitStatus::Rejected);
        if !already_resolved && should_pause(human_confirm, u.ord) {
            return pause_wrapped_run(
                store,
                &session,
                &workflow_id,
                u.ord,
                problem,
                entity_mode,
                &collection_scope,
                governance_mode,
                sandbox_root,
                timeout,
                &clis,
                human_confirm,
                outcomes,
            );
        }

        let outcome = execute_one_unit_wrapped(
            store,
            u,
            &clis,
            &cli_keys,
            &workflow_id,
            entity_mode,
            &session_id,
            governance_mode,
            sandbox_root,
            timeout,
        )?;
        outcomes.push(outcome);
    }

    // ── 4. session.completed. ──
    session.status = SessionStatus::Completed;
    put_node(store, session.to_node())?;
    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;
    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_SESSION_COMPLETED,
        "wicked-agent",
        "agent.session",
        serde_json::json!({ "session_id": session_id, "units": outcomes.len(), "approved": approved, "rejected": rejected }),
    ));

    Ok(SessionResult {
        session_id,
        workflow_id,
        entity_mode,
        collection_scope,
        units: outcomes,
        approved,
        rejected,
        paused_at: None,
    })
}

/// Decide whether the human-confirm gate pauses BEFORE a unit of the given `ord`.
fn should_pause(human_confirm: HumanConfirm, ord: u32) -> bool {
    match human_confirm {
        HumanConfirm::None => false,
        HumanConfirm::All => true,
        HumanConfirm::Before(target) => ord == target,
    }
}

/// Execute ONE unit on the REAL wrapped-CLI path, end to end, on the shared `store`:
/// `wrapped_cli_for` → [`execute::execute_unit_wrapped`] → evaluator≠creator second pass →
/// [`wicked_orchestration::tick_workflow`] → update the unit node fields/status → emit
/// `EV_AGENT_TASK_COMPLETED`. Returns the [`UnitOutcome`].
///
/// Both [`run_session_wrapped`] and [`resume_session`] call this so the per-unit behavior is
/// IDENTICAL on both paths (the logic is the byte-for-byte equivalent of the original execute-loop
/// body).
#[allow(clippy::too_many_arguments)]
fn execute_one_unit_wrapped(
    store: &mut SqliteStore,
    u: &mut WorkUnit,
    clis: &[wicked_council::AgenticCli],
    cli_keys: &[String],
    workflow_id: &str,
    entity_mode: EntityMode,
    session_id: &str,
    governance_mode: inject::GovernanceMode,
    sandbox_root: &std::path::Path,
    timeout: std::time::Duration,
) -> anyhow::Result<UnitOutcome> {
    use wicked_apps_core::emit::{emit_event, EmitEvent};

    let assigned = u
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let wrapped = wrapped_cli_for(&assigned, clis, governance_mode);
    let workdir = sandbox_root.join(session_id).join(&u.id);
    let mut outcome = execute::execute_unit_wrapped(
        store,
        u,
        &wrapped,
        workflow_id,
        entity_mode,
        session_id,
        &workdir,
        timeout,
    )?;

    // evaluator≠creator: second governance pass on approved units.
    if outcome.approved {
        let evaluator_cli = next_cli_in_roster(&outcome.assigned_cli, cli_keys);
        let eval_at = execute::EVAL_AT_BASE + u.ord as i64 + 1_000_000;
        if let Ok(eval) = execute::evaluate_unit(
            store,
            u,
            &outcome
                .artifact_path
                .clone()
                .unwrap_or_else(|| format!("stub-output for {}", u.description)),
            &evaluator_cli,
            &outcome.collection_scope,
            &format!("unit-{}", u.ord),
            eval_at,
        ) {
            outcome.evaluator_claim_id = Some(eval.claim_id);
        }
    }

    // Tick the workflow cursor.
    wicked_orchestration::tick_workflow(store, workflow_id, outcome.approved)?;

    u.phase_ref = Some(outcome.phase_id.clone());
    u.conformance_ref = outcome.claim_id.clone();
    u.phase_status = Some(outcome.phase_status.clone());
    u.collection_scope = Some(outcome.collection_scope.clone());
    u.status = if outcome.approved {
        UnitStatus::Done
    } else {
        UnitStatus::Rejected
    };
    put_node(store, u.to_node())?;
    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_TASK_COMPLETED,
        "wicked-agent",
        "agent.task",
        serde_json::json!({ "session_id": session_id, "unit_id": u.id, "phase_status": outcome.phase_status, "decision": outcome.decision, "gate_blocked": outcome.gate_blocked }),
    ));
    Ok(outcome)
}

/// PAUSE the wrapped run BEFORE the unit `next_ord`: persist a [`ResumeCursor`] (overwrite), set the
/// session + workflow to `AwaitingHuman`, and return a `SessionResult` with `paused_at = Some(ord)`.
/// The workflow cursor is NOT advanced past the un-executed unit (it stays consistent for resume).
#[allow(clippy::too_many_arguments)]
fn pause_wrapped_run(
    store: &mut SqliteStore,
    session: &AgentSession,
    workflow_id: &str,
    next_ord: u32,
    problem: &str,
    entity_mode: EntityMode,
    collection_scope: &Option<String>,
    governance_mode: inject::GovernanceMode,
    sandbox_root: &std::path::Path,
    timeout: std::time::Duration,
    clis: &[wicked_council::AgenticCli],
    human_confirm: HumanConfirm,
    outcomes: Vec<UnitOutcome>,
) -> anyhow::Result<SessionResult> {
    // Persist the FULL resume cursor (overwrite, not append) — the full Vec<AgenticCli> so
    // `wrapped_cli_for` can rebuild the subprocess commands on resume.
    let cursor = resume::ResumeCursor {
        session_id: session.id.clone(),
        workflow_id: workflow_id.to_string(),
        next_ord,
        problem: problem.to_string(),
        entity_mode,
        collection_scope: collection_scope.clone(),
        governance_mode,
        sandbox_root: sandbox_root.display().to_string(),
        timeout_secs: timeout.as_secs(),
        clis: clis.to_vec(),
        human_confirm,
    };
    resume::put_cursor(store, &cursor)?;

    // Set the session to AwaitingHuman and persist it.
    let mut paused_session = session.clone();
    paused_session.status = SessionStatus::AwaitingHuman;
    put_node(store, paused_session.to_node())?;

    // Set the workflow status to AwaitingHuman via the orchestration public API and re-persist.
    // (The harness's tick_workflow never sets AwaitingHuman, so we set it directly.)
    if let Some(mut wf) = wicked_orchestration::get_workflow(store, workflow_id)? {
        wf.status = wicked_orchestration::WorkflowStatus::AwaitingHuman;
        put_node(store, wf.to_node())?;
    }

    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;
    Ok(SessionResult {
        session_id: session.id.clone(),
        workflow_id: workflow_id.to_string(),
        entity_mode,
        collection_scope: collection_scope.clone(),
        units: outcomes,
        approved,
        rejected,
        paused_at: Some(next_ord),
    })
}

/// RESUME a paused wrapped session from its persisted [`ResumeCursor`].
///
/// Loads the cursor (via [`resume::get_cursor`]), re-reads the persisted units (they already carry
/// their council assignments — resume does NOT re-plan or re-distribute), and continues the execute
/// loop from the cursor's `next_ord`, SKIPPING units already `Done`/`Rejected`. The cursor's FULL
/// `Vec<AgenticCli>` rebuilds each subprocess command via `wrapped_cli_for`. The cursor's
/// `human_confirm` is honored, so resume can pause AGAIN (persisting a fresh cursor — overwrite).
///
/// On reaching the end: session → `Completed`, workflow → `Complete`, `paused_at = None`. Idempotent:
/// resuming a session with no cursor (already fully complete) returns the completed result WITHOUT
/// re-executing or double-ticking the workflow.
pub fn resume_session(store: &mut SqliteStore, session_id: &str) -> anyhow::Result<SessionResult> {
    use wicked_apps_core::emit::{emit_event, EmitEvent};

    let session = get_session(store, session_id)?
        .ok_or_else(|| anyhow::anyhow!("no session {session_id:?} found on the store"))?;
    let workflow_id = session.workflow_id.clone();

    // IDEMPOTENT no-op: an already-`Completed` session re-derives its completed result from the
    // persisted units WITHOUT re-executing or re-ticking the workflow. (The session status is the
    // authoritative completion signal; any lingering cursor is ignored.)
    if session.status == SessionStatus::Completed {
        let units = session_units(store, session_id)?;
        let outcomes: Vec<UnitOutcome> = units.iter().map(unit_outcome_from_node).collect();
        let approved = outcomes.iter().filter(|o| o.approved).count();
        let rejected = outcomes.len() - approved;
        return Ok(SessionResult {
            session_id: session.id,
            workflow_id,
            entity_mode: session.entity_mode,
            collection_scope: session.collection_scope,
            units: outcomes,
            approved,
            rejected,
            paused_at: None,
        });
    }

    // No cursor + not Completed ⇒ nothing to resume from (e.g. a stub session, or a never-paused
    // wrapped session). Treat as a no-op completed read so resume is total/safe.
    let Some(cursor) = resume::get_cursor(store, session_id)? else {
        let units = session_units(store, session_id)?;
        let outcomes: Vec<UnitOutcome> = units.iter().map(unit_outcome_from_node).collect();
        let approved = outcomes.iter().filter(|o| o.approved).count();
        let rejected = outcomes.len() - approved;
        return Ok(SessionResult {
            session_id: session.id,
            workflow_id,
            entity_mode: session.entity_mode,
            collection_scope: session.collection_scope,
            units: outcomes,
            approved,
            rejected,
            paused_at: None,
        });
    };

    let clis = cursor.clis.clone();
    let cli_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let entity_mode = cursor.entity_mode;
    let governance_mode = cursor.governance_mode;
    let sandbox_root = std::path::PathBuf::from(&cursor.sandbox_root);
    let timeout = std::time::Duration::from_secs(cursor.timeout_secs);
    let human_confirm = cursor.human_confirm;

    // Re-mark the session as Executing for the resumed run (it was AwaitingHuman).
    let mut session = session;
    session.status = SessionStatus::Executing;
    put_node(store, session.to_node())?;

    // Re-read the persisted units (they carry their assignments — do NOT re-distribute).
    let mut units = session_units(store, session_id)?;

    // Outcomes already executed on prior runs (persisted on the unit nodes) come first, in order.
    let mut outcomes: Vec<UnitOutcome> = Vec::with_capacity(units.len());
    for u in &units {
        if matches!(u.status, UnitStatus::Done | UnitStatus::Rejected) {
            outcomes.push(unit_outcome_from_node(u));
        }
    }

    // Continue the loop from next_ord, skipping already-resolved units, honoring human_confirm.
    for u in &mut units {
        if u.ord < cursor.next_ord {
            continue;
        }
        let already_resolved = matches!(u.status, UnitStatus::Done | UnitStatus::Rejected);
        if already_resolved {
            continue;
        }
        if should_pause(human_confirm, u.ord) {
            return pause_wrapped_run(
                store,
                &session,
                &workflow_id,
                u.ord,
                &cursor.problem,
                entity_mode,
                &cursor.collection_scope,
                governance_mode,
                &sandbox_root,
                timeout,
                &clis,
                human_confirm,
                outcomes,
            );
        }

        let outcome = execute_one_unit_wrapped(
            store,
            u,
            &clis,
            &cli_keys,
            &workflow_id,
            entity_mode,
            session_id,
            governance_mode,
            &sandbox_root,
            timeout,
        )?;
        outcomes.push(outcome);
    }

    // ── Completion: session → Completed (the authoritative completion signal for idempotency), the
    //    workflow is already Complete via the last unit's tick_workflow, event emitted. ──
    session.status = SessionStatus::Completed;
    put_node(store, session.to_node())?;

    let approved = outcomes.iter().filter(|o| o.approved).count();
    let rejected = outcomes.len() - approved;
    let _ = emit_event(&EmitEvent::new(
        EV_AGENT_SESSION_COMPLETED,
        "wicked-agent",
        "agent.session",
        serde_json::json!({ "session_id": session_id, "units": outcomes.len(), "approved": approved, "rejected": rejected }),
    ));

    Ok(SessionResult {
        session_id: session.id,
        workflow_id,
        entity_mode,
        collection_scope: cursor.collection_scope,
        units: outcomes,
        approved,
        rejected,
        paused_at: None,
    })
}

/// Reconstruct a [`UnitOutcome`] summary from a persisted (already-executed) [`WorkUnit`] node, for
/// the resume path's "outcomes already done on prior runs" list. Mirrors what the original run
/// recorded onto the node; fields the node does not carry (cli_exit_code, artifact_path) are `None`.
fn unit_outcome_from_node(u: &WorkUnit) -> UnitOutcome {
    let approved = matches!(u.status, UnitStatus::Done);
    UnitOutcome {
        unit_id: u.id.clone(),
        ord: u.ord,
        assigned_cli: u.assigned_cli.clone().unwrap_or_default(),
        phase_id: u
            .phase_ref
            .clone()
            .unwrap_or_else(|| format!("{}:unit-{}", u.session_id, u.ord)),
        phase_status: u
            .phase_status
            .clone()
            .unwrap_or_else(|| if approved { "approved" } else { "rejected" }.to_string()),
        decision: None,
        claim_id: u.conformance_ref.clone(),
        collection_scope: u.collection_scope.clone().unwrap_or_default(),
        approved,
        cli_exit_code: None,
        artifact_path: None,
        gate_blocked: false,
        evaluator_claim_id: None,
    }
}

/// Map a council-assigned CLI key back to a launchable [`WrappedCli`] using the convened roster.
///
/// The roster's [`wicked_council::AgenticCli`] carries the command vocabulary; we tokenize its
/// `headless_invocation` (the same whitespace+quote tokenizer the council dispatcher uses) to get
/// the program + leading args, dropping the trailing `{PROMPT}`/task-file placeholder (the launcher
/// appends the real TASK.txt). A `{PROMPT}` placeholder is dropped — the wrapped-CLI contract is
/// `command [args...] <TASK.txt>`, not a prompt substitution.
fn wrapped_cli_for(
    key: &str,
    clis: &[wicked_council::AgenticCli],
    mode: inject::GovernanceMode,
) -> inject::WrappedCli {
    let record = clis.iter().find(|c| c.key == key);
    let (command, args) = match record {
        Some(c) => {
            let mut toks = tokenize_invocation(&c.headless_invocation);
            // Drop any {PROMPT}/task placeholder token (the launcher appends the real task file).
            toks.retain(|t| !t.contains("{PROMPT}"));
            if toks.is_empty() {
                (c.binary.clone(), Vec::new())
            } else {
                let program = toks.remove(0);
                (program, toks)
            }
        }
        None => (key.to_string(), Vec::new()),
    };
    inject::WrappedCli {
        command,
        args,
        mode,
        id: key.to_string(),
    }
}

/// Whitespace tokenizer that keeps double-quoted spans together and strips the surrounding quotes
/// (same shape as the council dispatcher's `tokenize`). Good enough for the registry templates.
fn tokenize_invocation(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                any = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Pick an evaluator CLI that is DIFFERENT from `creator`. Returns the next key in the roster, or
/// the first if creator is last, or `"wicked-evaluator"` as a synthetic fallback when the roster
/// has only one seat. The caller uses the returned key to stamp the evaluator_identity.
fn next_cli_in_roster(creator: &str, roster: &[String]) -> String {
    let pos = roster.iter().position(|k| k == creator);
    match pos {
        Some(i) => roster
            .get(i + 1)
            .or_else(|| roster.first())
            .filter(|k| k.as_str() != creator) // don't loop back to the same when only 1 seat
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
        None => roster
            .first()
            .cloned()
            .unwrap_or_else(|| "wicked-evaluator".to_string()),
    }
}

/// A deterministic short id from parts (sha256 prefix; matches the estate's dependency-free style).
pub(crate) fn deterministic_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(parts.join("|").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use wicked_apps_core::SqliteStore;
    use wicked_council::types::{
        AgenticCli, Category, Confidence, InputMode, RankSignal, RankStore,
    };
    use wicked_council::{EstateHandle, EstateRankStore};
    use wicked_governance::{
        claim_from_node, claim_symbol, Effect, Policy, Severity, Trigger, EVALUATOR_IDENTITY,
    };
    use wicked_orchestration::{get_workflow, WorkflowStatus};

    use super::*;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn unique_tempdir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("wa-test-{tag}-{pid}-{n}"));
        std::fs::create_dir_all(&dir).expect("create tempdir");
        dir
    }

    fn write_fake_cli(dir: &std::path::Path, name: &str, recommendation: &str) -> String {
        let path = dir.join(name);
        let script = format!(
            "#!/bin/sh\n\
             echo \"RECOMMENDATION: {recommendation}\"\n\
             echo \"TOP_RISK: none\"\n\
             echo \"CHANGE_MY_MIND: no\"\n\
             echo \"DISQUALIFIER: None\"\n"
        );
        std::fs::write(&path, &script).expect("write fake cli");
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        path.display().to_string()
    }

    fn fake_cli_record(key: &str, script_path: &str) -> AgenticCli {
        AgenticCli {
            key: key.to_string(),
            display_name: format!("Fake {key}"),
            binary: script_path.to_string(),
            headless_invocation: format!("{script_path} \"{{PROMPT}}\""),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
        }
    }

    fn two_fake_clis(dir: &std::path::Path) -> Vec<AgenticCli> {
        let a = write_fake_cli(dir, "fake-a.sh", "fake-a");
        let b = write_fake_cli(dir, "fake-b.sh", "fake-b");
        vec![fake_cli_record("fake-a", &a), fake_cli_record("fake-b", &b)]
    }

    // ── Tests ─────────────────────────────────────────────────────────────────

    /// After a complete run, the Workflow node must have cursor == unit_count and status == Complete.
    #[test]
    fn run_session_workflow_cursor_reaches_complete() {
        let dir = unique_tempdir("wf-cursor");
        let clis = two_fake_clis(&dir);

        let mut store = SqliteStore::in_memory().expect("open store");
        let result = run_session(
            &mut store,
            clis,
            "design\nimplement",
            EntityMode::Shared,
            Some("sess-wf-cursor"),
        )
        .expect("run_session");

        let wf = get_workflow(&store, &result.workflow_id)
            .expect("read workflow")
            .expect("workflow must exist");

        assert_eq!(
            wf.status,
            WorkflowStatus::Complete,
            "all units approved → workflow must be Complete"
        );
        assert_eq!(
            wf.current_index,
            result.units.len(),
            "cursor must have advanced past every unit"
        );
    }

    /// Approved units must carry a distinct evaluator_claim_id (evaluator≠creator invariant).
    #[test]
    fn approved_unit_has_evaluator_claim_distinct_from_creator() {
        let dir = unique_tempdir("eval-claim");
        let clis = two_fake_clis(&dir);

        let mut store = SqliteStore::in_memory().expect("open store");
        let result = run_session(
            &mut store,
            clis,
            "single task",
            EntityMode::Shared,
            Some("sess-eval-claim"),
        )
        .expect("run_session");

        // All units in this no-policy session should be approved.
        for outcome in &result.units {
            assert!(
                outcome.approved,
                "unit {} should be approved",
                outcome.unit_id
            );
            assert!(
                outcome.evaluator_claim_id.is_some(),
                "approved unit {} must have an evaluator_claim_id",
                outcome.unit_id
            );
            assert_ne!(
                outcome.evaluator_claim_id.as_deref(),
                outcome.claim_id.as_deref(),
                "evaluator_claim_id must differ from the creator claim_id (different seed)"
            );
        }
    }

    /// The creator claim must bear the canonical EVALUATOR_IDENTITY; the evaluator claim must
    /// bear a "wicked-evaluator:…" identity — these are the two halves of the evaluator≠creator proof.
    #[test]
    fn evaluator_identity_is_distinct_from_creator_identity() {
        let dir = unique_tempdir("eval-id");
        let clis = two_fake_clis(&dir);

        let mut store = SqliteStore::in_memory().expect("open store");
        let result = run_session(
            &mut store,
            clis,
            "single unit",
            EntityMode::Shared,
            Some("sess-eval-id"),
        )
        .expect("run_session");

        let outcome = result.units.first().expect("at least one unit");
        assert!(outcome.approved);

        // Read both claims from the store and check their evaluator_identity fields.
        let creator_claim_id = outcome.claim_id.as_deref().expect("creator claim_id");
        let evaluator_claim_id = outcome
            .evaluator_claim_id
            .as_deref()
            .expect("evaluator claim_id");

        let creator_node = store
            .get_node(&claim_symbol(creator_claim_id))
            .expect("store ok")
            .expect("creator claim node must exist");
        let creator_claim = claim_from_node(&creator_node).expect("parse creator");

        let evaluator_node = store
            .get_node(&claim_symbol(evaluator_claim_id))
            .expect("store ok")
            .expect("evaluator claim node must exist");
        let evaluator_claim = claim_from_node(&evaluator_node).expect("parse evaluator");

        assert_eq!(
            creator_claim.evaluator_identity, EVALUATOR_IDENTITY,
            "creator pass must bear the canonical EVALUATOR_IDENTITY"
        );
        assert!(
            evaluator_claim
                .evaluator_identity
                .starts_with("wicked-evaluator:"),
            "evaluator pass must bear a wicked-evaluator:… identity, got {:?}",
            evaluator_claim.evaluator_identity
        );
        assert_ne!(
            creator_claim.evaluator_identity, evaluator_claim.evaluator_identity,
            "creator and evaluator identities must differ"
        );
    }

    /// Rejected units must NOT get an evaluator claim (evaluation only runs on approval).
    #[test]
    fn rejected_unit_has_no_evaluator_claim() {
        let dir = unique_tempdir("reject-eval");
        let clis = two_fake_clis(&dir);

        let mut store = SqliteStore::in_memory().expect("open store");

        // Deny policy targeting the first unit's phase ("unit-1" for a single-piece problem).
        let deny_policy = Policy {
            id: "deny-all-test".to_string(),
            kind: "policy".to_string(),
            applies_to: vec!["unit-1".to_string()],
            effect: Effect::Deny,
            trigger: Trigger { contains: None },
            obligations: vec![],
            criteria: "deny unit-1".to_string(),
            severity: Severity::High,
            rule: "test deny".to_string(),
        };
        wicked_governance::register_policy(&mut store, &deny_policy).expect("register deny policy");

        let result = run_session(
            &mut store,
            clis,
            "some task",
            EntityMode::Shared,
            Some("sess-reject-eval"),
        )
        .expect("run_session");

        for outcome in &result.units {
            assert!(!outcome.approved, "deny policy must reject the unit");
            assert!(
                outcome.evaluator_claim_id.is_none(),
                "rejected unit must NOT have an evaluator_claim_id"
            );
        }
    }

    /// Rejection ticks the workflow to Failed — the cursor stops at the rejected phase.
    #[test]
    fn rejected_session_workflow_is_failed() {
        let dir = unique_tempdir("wf-fail");
        let clis = two_fake_clis(&dir);

        let mut store = SqliteStore::in_memory().expect("open store");

        let deny_policy = Policy {
            id: "deny-all-fail".to_string(),
            kind: "policy".to_string(),
            applies_to: vec!["unit-1".to_string()],
            effect: Effect::Deny,
            trigger: Trigger { contains: None },
            obligations: vec![],
            criteria: "deny unit-1".to_string(),
            severity: Severity::High,
            rule: "test deny".to_string(),
        };
        wicked_governance::register_policy(&mut store, &deny_policy).expect("register policy");

        let result = run_session(
            &mut store,
            clis,
            "fail this",
            EntityMode::Shared,
            Some("sess-wf-fail"),
        )
        .expect("run_session");

        let wf = get_workflow(&store, &result.workflow_id)
            .expect("read workflow")
            .expect("workflow must exist");

        assert_eq!(
            wf.status,
            WorkflowStatus::Failed,
            "a denied unit must set the workflow to Failed"
        );
        // tick_workflow does NOT advance the cursor on rejection — it stays at 0.
        assert_eq!(wf.current_index, 0, "cursor is not advanced on rejection");
    }

    /// Adversarial: the ranked fast path bypasses the COUNCIL but the GOVERNANCE GATE still fires.
    /// Seed sufficient historical observations for fake-a → distribute must fast-path (no
    /// council_task_ref) → execute must still produce a claim (gate is not bypassed).
    #[test]
    fn ranked_fast_path_bypasses_council_but_not_governance_gate() {
        let dir = unique_tempdir("ranked");
        let clis = two_fake_clis(&dir);
        let db_path = dir.join("ranked.db");
        let db_str = db_path.display().to_string();

        // Seed 6 observations for "fake-a" on the shared store so the fast path fires.
        {
            let estate = EstateHandle::new(SqliteStore::open(&db_str).expect("open ranked db"));
            let rank_store = EstateRankStore::new(estate);
            let work_kind = wicked_council::work_kind_for(&["general".to_string()]);
            for _ in 0..6 {
                rank_store.record(
                    "fake-a",
                    &work_kind,
                    &RankSignal {
                        success: true,
                        agreement_with_consensus: true,
                        latency_ms: 100,
                    },
                );
            }
        }

        // Distribute: with 6 observations at score 1.0 the fast path fires → no council_task_ref.
        let units = plan::plan_units("ranked task", "sess-ranked");
        let dists = distribute::distribute_units_on(&units, &clis, "sess-ranked", Some(&db_str))
            .expect("distribute");
        assert!(
            dists.iter().all(|d| d.council_task_ref.is_none()),
            "ranked fast path must fire: no council_task_ref when score ≥ 0.80 with ≥ 5 obs"
        );
        assert!(
            dists.iter().all(|d| d.assigned_cli == "fake-a"),
            "fast path must assign the ranked winner (fake-a)"
        );

        // Execute: the governance gate STILL fires even though distribution skipped the council.
        let mut exec_store = SqliteStore::in_memory().expect("open exec store");
        let mut unit = units.into_iter().next().expect("at least one unit");
        unit.assigned_cli = Some("fake-a".to_string());
        let outcome = execute::execute_unit(
            &mut exec_store,
            &unit,
            "wf-sess-ranked",
            EntityMode::Shared,
            "sess-ranked",
        )
        .expect("execute_unit");

        assert!(
            outcome.claim_id.is_some(),
            "governance gate must fire even on ranked-fast-path distribution"
        );
    }
}
