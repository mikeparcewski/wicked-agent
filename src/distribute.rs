//! DISTRIBUTE — convene `wicked_council` IN-PROCESS to pick the CLI assigned to each unit.
//!
//! Ported from the prototype's distribute step (`session.mjs` §3 + `clients/council.mjs`). For each
//! unit the harness convenes the council over the session's roster and reads the verdict:
//!
//! - `queue` the council task, then `poll` the (now-resolved) verdict — the REAL council worker
//!   path. We use [`wicked_council::Worker::queue_blocking`] (queue + join the detached worker
//!   thread) so distribution is deterministic for the harness without a poll loop; the production
//!   contract is the non-blocking `queue`/`poll`, which `queue_blocking` is built on.
//! - The verdict's `winning_recommendation` names the assigned CLI when it matches a roster seat;
//!   otherwise the harness gracefully degrades to the first usable seat (NEVER fails the unit on a
//!   no-consensus / split council — distribution must always produce an assignment).
//!
//! ## Store note
//! The council persists its own task/verdict/ranking through its internal `EstateHandle` ledger
//! (exercised by the council's OWN E2E). The agent's shared-collection contract is the work-unit
//! node, onto which the harness records the assignment (see `lib::run_session`). The council is
//! driven in-process here; its decision flows back into the shared store via that unit node.

use std::sync::Arc;
use std::time::Duration;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, TaskState, Worker,
};

use crate::WorkUnit;

/// The distribution decision for one unit.
#[derive(Debug, Clone)]
pub struct Distribution {
    /// The unit this distribution is for.
    pub unit_id: String,
    /// The CLI the council assigned.
    pub assigned_cli: String,
    /// The council task id whose verdict produced the assignment (provenance).
    pub council_task_ref: Option<String>,
    /// Whether the assignment was a graceful degrade (no usable verdict winner ⇒ first seat).
    pub degraded: bool,
}

/// The criteria the council weighs when distributing a unit. A single coarse bucket — the harness
/// does not invent a taxonomy the caller didn't provide (mirrors the prototype's `["general"]`).
const DISTRIBUTE_CRITERIA: &[&str] = &["general"];

/// Convene the council (in-process) for every unit and return one [`Distribution`] each.
///
/// `clis` is the convened roster (already probed-usable by the caller — fake seats in tests, real
/// in R6). The council runs over a NoopEventSink (the emit seam shells to the bus, which the
/// hermetic flow must not require). The council persists its task/verdict to its OWN in-memory
/// ledger here (the council's internal concern) — see [`distribute_units_on`] to share an on-disk
/// store so the council's task + verdict land on the SAME file as the agent (R6).
pub fn distribute_units(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
) -> anyhow::Result<Vec<Distribution>> {
    distribute_units_on(units, clis, session_id, None)
}

/// As [`distribute_units`], but when `db_path` is `Some`, the council convenes over an estate handle
/// opened on THAT on-disk file — so its `COUNCIL_TASK` + `COUNCIL_VERDICT` (+ `CLI_RANKING`) nodes
/// persist on the SAME shared store as the agent/governance/orchestration entities (the R6 single-
/// file invariant). Distribution is sequential and each council `queue_blocking` joins its worker
/// before returning, so the council's connection never writes concurrently with the agent's idle
/// handle (WAL, one-writer-at-a-time). `None` keeps the council on its own in-memory ledger.
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
) -> anyhow::Result<Vec<Distribution>> {
    let roster_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();

    units
        .iter()
        .map(|unit| distribute_one(unit, clis, &roster_keys, session_id, db_path))
        .collect()
}

fn distribute_one(
    unit: &WorkUnit,
    clis: &[AgenticCli],
    roster_keys: &[String],
    session_id: &str,
    db_path: Option<&str>,
) -> anyhow::Result<Distribution> {
    // The council convenes over an estate ledger: on-disk (the SHARED file, so its task/verdict land
    // alongside the agent's entities — R6) when `db_path` is given, else its own in-memory ledger.
    let estate = match db_path {
        Some(path) => EstateHandle::new(
            apps_core::SqliteStore::open(path)
                .map_err(|e| anyhow::anyhow!("open council estate on {path}: {e}"))?,
        ),
        None => EstateHandle::in_memory()
            .map_err(|e| anyhow::anyhow!("open council estate handle: {e}"))?,
    };
    let ledger = Ledger::new(estate.clone());
    let rank_store = Arc::new(EstateRankStore::new(estate));
    let dispatcher = Arc::new(RealDispatcher {
        timeout: Duration::from_secs(30),
        local_runner_timeout: Duration::from_secs(30),
    });
    let criteria: Vec<String> = DISTRIBUTE_CRITERIA.iter().map(|s| s.to_string()).collect();
    let work_kind = work_kind_for(&criteria);

    let worker = Worker::new(
        ledger,
        dispatcher,
        rank_store,
        Arc::new(NoopEventSink),
        clis.to_vec(),
        work_kind,
    );

    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!("which CLI should own work unit {}: {}", unit.id, unit.description),
        // The options are the convened seats — the council weighs WHICH seat owns the unit.
        options: roster_keys.to_vec(),
        criteria,
        session_id: session_id.to_string(),
    };
    let task_id = worker.queue_blocking(task);

    let status: Option<PollStatus> = worker.poll(&task_id);
    let (assigned_cli, degraded) = pick_assignment(status.as_ref(), roster_keys);

    Ok(Distribution {
        unit_id: unit.id.clone(),
        assigned_cli,
        council_task_ref: Some(task_id),
        degraded,
    })
}

/// Pick the assigned CLI from the council's poll status.
///
/// Preference order:
/// 1. The verdict's `winning_recommendation`, IF it names a roster seat (exact or contained match).
/// 2. Graceful degrade: the first roster seat (a no-consensus / split / failed council must NEVER
///    leave a unit unassigned — distribution always produces an assignment).
fn pick_assignment(status: Option<&PollStatus>, roster_keys: &[String]) -> (String, bool) {
    let fallback = || {
        roster_keys
            .first()
            .cloned()
            .unwrap_or_else(|| "claude".to_string())
    };

    let Some(status) = status else {
        return (fallback(), true);
    };
    if status.state != TaskState::Voted {
        return (fallback(), true);
    }
    let Some(verdict) = &status.verdict else {
        return (fallback(), true);
    };
    let Some(winner) = &verdict.winning_recommendation else {
        return (fallback(), true);
    };

    // Match the verdict winner to a roster seat. Fake CLIs recommend a seat key directly; real CLIs
    // recommend prose, so accept an exact key match OR a roster key contained in the recommendation.
    let winner_norm = winner.to_lowercase();
    if let Some(seat) = roster_keys
        .iter()
        .find(|k| winner_norm == k.to_lowercase() || winner_norm.contains(&k.to_lowercase()))
    {
        (seat.clone(), false)
    } else {
        (fallback(), true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_council::Verdict;

    fn status_with_winner(winner: Option<&str>, state: TaskState) -> PollStatus {
        PollStatus {
            task_id: "t".into(),
            state,
            returned: 1,
            pending: 0,
            verdict: winner.map(|w| Verdict {
                task_id: "t".into(),
                kind: "Consensus".into(),
                consensus: true,
                winning_recommendation: Some(w.to_string()),
                agreement_ratio: 1.0,
                risk_convergence: vec![],
                dissent: vec![],
            }),
        }
    }

    #[test]
    fn winner_matching_a_seat_is_assigned() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let st = status_with_winner(Some("fake-b"), TaskState::Voted);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "fake-b");
        assert!(!degraded, "an exact seat winner is not a degrade");
    }

    #[test]
    fn winner_contained_in_prose_recommendation_matches_seat() {
        let roster = vec!["claude".to_string(), "agy".to_string()];
        let st = status_with_winner(Some("Use claude for its strong refactoring"), TaskState::Voted);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "claude");
        assert!(!degraded);
    }

    #[test]
    fn no_verdict_or_no_match_degrades_to_first_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        // No status at all.
        let (cli, degraded) = pick_assignment(None, &roster);
        assert_eq!(cli, "fake-a");
        assert!(degraded);
        // Voted but winner names nothing in the roster.
        let st = status_with_winner(Some("Option Z"), TaskState::Voted);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "fake-a");
        assert!(degraded, "a winner that matches no seat is a graceful degrade");
        // Failed council (no verdict).
        let st = status_with_winner(None, TaskState::Failed);
        let (cli, degraded) = pick_assignment(Some(&st), &roster);
        assert_eq!(cli, "fake-a");
        assert!(degraded);
    }
}
