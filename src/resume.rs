//! RESUME — the pause/resume cursor for the REAL wrapped-CLI run path.
//!
//! When [`run_session_wrapped`](crate::run_session_wrapped) PAUSES before a unit (the
//! [`HumanConfirm`](crate::HumanConfirm) gate), it persists a [`ResumeCursor`] onto the shared
//! estate store. `wicked-agent resume <session_id>` (or [`resume_session`](crate::resume_session))
//! reads the cursor back and continues the execute loop from `next_ord` — re-launching the
//! council-assigned CLIs as real subprocesses, idempotent over already-Done/Rejected units.
//!
//! ## THE INVARIANT (why the cursor exists)
//! The cursor carries the FULL `Vec<AgenticCli>` roster (each seat's `binary` +
//! `headless_invocation` + `key`) — NOT just the seat keys. [`crate::run_session_wrapped`] rebuilds
//! each unit's subprocess command via `wrapped_cli_for`, which needs the seat's command vocabulary.
//! The persisted [`AgentSession`](crate::AgentSession) only stores the seat KEYS (a `Vec<String>`),
//! which is insufficient to rebuild the launch — so the resume cursor persists the whole roster.

use serde::{Deserialize, Serialize};
use wicked_apps_core::{
    synthetic_symbol, FromNode, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
    ToNode, SYMBOL_SCHEME,
};

use crate::inject::GovernanceMode;
use crate::scope::EntityMode;
use crate::HumanConfirm;

/// Node-kind for a persisted resume cursor (`wicked-agent`). One cursor per session id; writing a
/// fresh cursor OVERWRITES the previous one (the synthetic symbol is keyed by `session_id`).
pub const AGENT_RESUME_CURSOR: &str = "agent_resume_cursor";

/// A persisted resume cursor: everything [`resume_session`](crate::resume_session) needs to continue
/// a paused wrapped run from `next_ord`, WITHOUT re-planning or re-distributing.
///
/// Persisted as `Node(Other(AGENT_RESUME_CURSOR))` on the shared store, keyed by `session_id`. The
/// whole struct round-trips through `Node.metadata` (lossless, like [`AgentSession`](crate::AgentSession)).
///
/// (No `PartialEq` derive — `wicked_council::AgenticCli` does not implement `PartialEq`; compare the
/// relevant cursor fields directly when asserting in tests.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeCursor {
    /// The session this cursor belongs to (the node identity).
    pub session_id: String,
    /// The orchestration workflow backing the session.
    pub workflow_id: String,
    /// The `ord` of the next unit to execute — the loop resumes here, skipping already-resolved units.
    pub next_ord: u32,
    /// The original free-text problem (so resume re-derives the same session id if needed).
    pub problem: String,
    /// Shared vs isolated collection-scope mode (§6).
    pub entity_mode: EntityMode,
    /// The session-wide collection scope under shared mode (`None` under isolated).
    pub collection_scope: Option<String>,
    /// The per-tool-call governance mechanism for the launched CLIs.
    pub governance_mode: GovernanceMode,
    /// The sandbox root each unit's workdir is created under (absolute path string).
    pub sandbox_root: String,
    /// The per-subprocess timeout, in whole seconds.
    pub timeout_secs: u64,
    /// The FULL convened roster (binary + headless_invocation + key) — NOT just the keys. The whole
    /// reason the cursor exists: `wrapped_cli_for` needs these to rebuild the subprocess commands.
    pub clis: Vec<wicked_council::AgenticCli>,
    /// The human-confirm gate policy — honored on resume so the run can pause again.
    pub human_confirm: HumanConfirm,
}

impl ToNode for ResumeCursor {
    fn node_kind() -> &'static str {
        AGENT_RESUME_CURSOR
    }

    fn to_node(&self) -> Node {
        let mut node = Node::new(
            synthetic_symbol(AGENT_RESUME_CURSOR, &self.session_id),
            NodeKind::Other(AGENT_RESUME_CURSOR.to_string()),
            self.session_id.clone(),
            Language::new(SYMBOL_SCHEME),
            Location::new(
                format!("{AGENT_RESUME_CURSOR}/{}", self.session_id),
                Span::ZERO,
            ),
        );
        // The whole struct round-trips through one metadata object (lossless, no per-field plumbing).
        if let serde_json::Value::Object(map) =
            serde_json::to_value(self).expect("ResumeCursor serializes to JSON")
        {
            node.metadata = map;
        }
        node
    }
}

impl FromNode for ResumeCursor {
    fn from_node(node: &Node) -> anyhow::Result<Self> {
        match &node.kind {
            NodeKind::Other(k) if k == AGENT_RESUME_CURSOR => {}
            other => {
                anyhow::bail!("expected NodeKind::Other({AGENT_RESUME_CURSOR:?}), got {other:?}")
            }
        }
        serde_json::from_value(serde_json::Value::Object(node.metadata.clone()))
            .map_err(|e| anyhow::anyhow!("node {} is not a valid ResumeCursor: {e}", node.name))
    }
}

/// Persist (OVERWRITE) the resume cursor for a session on the shared store.
pub fn put_cursor(store: &mut SqliteStore, cursor: &ResumeCursor) -> anyhow::Result<()> {
    crate::put_node(store, cursor.to_node())
}

/// Read back the resume cursor for `session_id`, if one was persisted.
pub fn get_cursor(store: &dyn GraphRead, session_id: &str) -> anyhow::Result<Option<ResumeCursor>> {
    match store.get_node(&synthetic_symbol(AGENT_RESUME_CURSOR, session_id))? {
        Some(node) => Ok(Some(ResumeCursor::from_node(&node)?)),
        None => Ok(None),
    }
}
