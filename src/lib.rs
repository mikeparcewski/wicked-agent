//! wicked-agent — the in-process harness (R5 capstone) that drives **governance +
//! orchestration + council** on ONE shared wicked-estate store (the collection, finally real).
//!
//! This is the Rust port of the Node prototype (`wicked-agent/lib/session.mjs`, ARCHITECTURE §3:
//! the OWNED interactive flow) onto the wicked-apps spine (`apps-core`). [`run_session`] DRIVES the
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

use apps_core::{
    synthetic_symbol, FromNode, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
    ToNode, AGENT_SESSION, SYMBOL_SCHEME, WORK_UNIT,
};
use serde::{Deserialize, Serialize};

pub mod distribute;
pub mod execute;
pub mod inject;
pub mod plan;
pub mod scope;

pub use distribute::{distribute_units, distribute_units_on, Distribution};
pub use execute::{execute_unit, execute_unit_wrapped, UnitOutcome, DEFAULT_CLI_TIMEOUT, WORK_OUTPUT};
pub use inject::{
    launch_wrapped, run_gate_hook, GovernanceMode, LaunchOutcome, ToolCall, WrappedCli,
};
pub use plan::plan_units;
pub use scope::{resolve_scope, EntityMode};

// ─────────────────────────────────────────────────────────────────────────────
// Coarse bus events (counts / ids only) — mirrors the apps-core agent catalog.
// ─────────────────────────────────────────────────────────────────────────────

pub use apps_core::{
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
    Completed,
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
    pub fn pending(id: impl Into<String>, session_id: impl Into<String>, ord: u32, description: impl Into<String>) -> Self {
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
    use apps_core::GraphWrite;
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Read an [`AgentSession`] back from the shared store by id.
pub fn get_session(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Option<AgentSession>> {
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
    use apps_core::emit::{emit_event, EmitEvent};

    let session_id = session_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("sess-{}", deterministic_id(&[problem, &clis.len().to_string()])));
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
        let outcome = execute::execute_unit(store, u, &workflow_id, entity_mode, &session_id)?;

        // Record the unit's outcome back onto its shared-store node.
        u.phase_ref = Some(outcome.phase_id.clone());
        u.conformance_ref = outcome.claim_id.clone();
        u.phase_status = Some(outcome.phase_status.clone());
        u.collection_scope = Some(outcome.collection_scope.clone());
        u.status = if outcome.approved { UnitStatus::Done } else { UnitStatus::Rejected };
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
) -> anyhow::Result<SessionResult> {
    use apps_core::emit::{emit_event, EmitEvent};

    let session_id = session_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("sess-{}", deterministic_id(&[problem, &clis.len().to_string()])));
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

    // ── 2. DISTRIBUTE — the council (in-process, REAL verdict) picks the assigned CLI per unit. ──
    // The council shares the SAME on-disk file (its task/verdict land alongside the agent's entities,
    // R6) — resolved from WICKED_ESTATE_DB, which the caller exports for the gate-hook child too. The
    // agent's `store` handle is idle during distribution; the council uses its own connection.
    let db_path = std::env::var("WICKED_ESTATE_DB").ok().filter(|p| !p.is_empty() && p != ":memory:");
    let distributions = distribute::distribute_units_on(&units, &clis, &session_id, db_path.as_deref())?;
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
        let assigned = u.assigned_cli.clone().unwrap_or_else(|| "claude".to_string());
        let wrapped = wrapped_cli_for(&assigned, &clis, governance_mode);
        let workdir = sandbox_root.join(&session_id).join(&u.id);
        let outcome = execute::execute_unit_wrapped(
            store,
            u,
            &wrapped,
            &workflow_id,
            entity_mode,
            &session_id,
            &workdir,
            timeout,
        )?;

        u.phase_ref = Some(outcome.phase_id.clone());
        u.conformance_ref = outcome.claim_id.clone();
        u.phase_status = Some(outcome.phase_status.clone());
        u.collection_scope = Some(outcome.collection_scope.clone());
        u.status = if outcome.approved { UnitStatus::Done } else { UnitStatus::Rejected };
        put_node(store, u.to_node())?;
        let _ = emit_event(&EmitEvent::new(
            EV_AGENT_TASK_COMPLETED,
            "wicked-agent",
            "agent.task",
            serde_json::json!({ "session_id": session_id, "unit_id": u.id, "phase_status": outcome.phase_status, "decision": outcome.decision, "gate_blocked": outcome.gate_blocked }),
        ));
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
    })
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

/// A deterministic short id from parts (sha256 prefix; matches the estate's dependency-free style).
pub(crate) fn deterministic_id(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(parts.join("|").as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}
