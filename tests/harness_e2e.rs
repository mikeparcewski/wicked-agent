//! THE KEY PROOF — the capstone integration test (R5).
//!
//! In-process, ONE shared `SqliteStore::in_memory()`, the harness drives **governance +
//! orchestration + council** on that one store. We register a DENY governance policy that fires on
//! unit-1's context (it embeds an AWS access key) and NOT unit-2's, run a 2-unit session, and prove:
//!
//!   (a) unit-1's orchestration phase resolves to `Rejected` — the governance deny fired THROUGH
//!       the agent, in-process (the structural `gate_decision` veto, ADR-0003), NOT approved;
//!   (b) unit-2's phase resolves to `Approved` with a work-output node recorded;
//!   (c) the session node + both work-unit nodes + the ConformanceClaim node(s) + both phase nodes
//!       ALL persist on the SAME store (read back via get_node / find_symbols).
//!
//! Council distribution is deterministic + offline: fake-CLI **shell scripts** (Unix-gated, like
//! the council's own E2E) echo a canned vote naming a roster seat. The emit seam is pointed at a
//! guaranteed-missing bus program with a temp dead-letter spool so nothing touches a real bus / HOME.

use wicked_agent::execute::{get_work_output, WORK_OUTPUT};
use wicked_agent::scope::EntityMode;
use wicked_agent::{get_session, run_session, session_units, SessionStatus, UnitStatus};
use wicked_apps_core::{
    synthetic_symbol, GraphRead, NodeKind, SqliteStore, AGENT_SESSION, CONFORMANCE_CLAIM, PHASE,
    WORK_UNIT,
};
use wicked_council::AgenticCli;
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

/// An AWS access key id shaped to trip the deny policy's regex (`AKIA[0-9A-Z]{16}`).
const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

/// A deny policy that fires whenever the evaluated context embeds an AWS access key id, selected
/// for BOTH unit phases (`unit-1`, `unit-2`) so the gate fires on each — but its `contains` trigger
/// only matches unit-1's context (which carries the secret).
fn deny_secrets_policy() -> Policy {
    Policy {
        id: "pol-deny-secrets".to_string(),
        kind: "security".to_string(),
        applies_to: vec!["unit-1".to_string(), "unit-2".to_string()],
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some("AKIA[0-9A-Z]{16}".to_string()),
        },
        obligations: vec![],
        criteria: "no aws access keys in the unit's work".to_string(),
        severity: Severity::High,
        rule: "Deny any unit whose work embeds an AWS access key id.".to_string(),
    }
}

/// Point the emit seam at a guaranteed-missing bus program + a temp dead-letter spool, so the
/// harness's fire-and-forget events never touch a real bus or write under HOME during the test.
fn hermetic_emit() -> std::path::PathBuf {
    let spool = std::env::temp_dir().join(format!("wa-e2e-emit-{}.ndjson", std::process::id()));
    unsafe {
        std::env::set_var(
            wicked_apps_core::emit::EMIT_PROGRAM_ENV,
            "wicked-bus-absent-xyzzy-9000",
        );
        std::env::set_var(wicked_apps_core::emit::DEADLETTER_ENV, &spool);
    }
    spool
}

fn clear_emit(spool: &std::path::Path) {
    unsafe {
        std::env::remove_var(wicked_apps_core::emit::EMIT_PROGRAM_ENV);
        std::env::remove_var(wicked_apps_core::emit::DEADLETTER_ENV);
    }
    let _ = std::fs::remove_file(spool);
}

/// Write an executable fake-CLI shell script that echoes the four scaffold lines, recommending
/// `recommendation` (a roster seat key, so the harness assigns that seat deterministically).
#[cfg(unix)]
fn write_fake_cli(dir: &std::path::Path, name: &str, recommendation: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\n\
         echo \"RECOMMENDATION: {recommendation}\"\n\
         echo \"TOP_RISK: latency\"\n\
         echo \"CHANGE_MY_MIND: depends on the benchmark\"\n\
         echo \"DISQUALIFIER: None\"\n"
    );
    std::fs::write(&path, script).expect("write fake cli");
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path.display().to_string()
}

#[cfg(unix)]
fn fake_cli_record(key: &str, script_path: &str) -> AgenticCli {
    use wicked_council::{Category, Confidence, InputMode};
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

#[cfg(unix)]
fn unique_tempdir(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("wa-{tag}-{}-{nanos}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// THE PROOF: governance deny fires THROUGH the agent into an orchestration `Rejected` phase, in
/// process, on ONE shared store; unit-2 approves with a work-output node; and session/units/claim/
/// phases ALL persist on the SAME store.
#[cfg(unix)]
#[test]
fn governance_deny_fires_through_agent_and_everything_persists_on_one_store() {
    let dir = unique_tempdir("e2e");
    let spool = hermetic_emit();

    // ── ONE shared in-memory estate store: the collection, finally real. ──
    let mut store = SqliteStore::in_memory().expect("open the one shared in-memory store");

    // Register the DENY policy on the SHARED store (governance writes here, in-process).
    register_policy(&mut store, &deny_secrets_policy()).expect("register deny policy");

    // Two deterministic fake-CLI seats, both recommending "fake-a" → a stable assignment.
    let a = write_fake_cli(&dir, "fake-a.sh", "fake-a");
    let b = write_fake_cli(&dir, "fake-b.sh", "fake-a");
    let roster = vec![fake_cli_record("fake-a", &a), fake_cli_record("fake-b", &b)];

    // A 2-unit problem: unit-1's text EMBEDS the secret (trips the deny); unit-2's is clean.
    // The plan splits on the sentence terminator into exactly two units, in order.
    let problem = format!(
        "Ship the build but it embeds {SECRET} in the deploy script. Write the release notes for the docs site."
    );

    let result = run_session(
        &mut store,
        roster,
        &problem,
        EntityMode::Shared,
        Some("sess-e2e"),
    )
    .expect("run_session on the shared store");

    // ── The session ran to completion over exactly two units. ──
    assert_eq!(result.session_id, "sess-e2e");
    assert_eq!(
        result.units.len(),
        2,
        "the problem decomposes into two units"
    );
    assert_eq!(
        result.approved, 1,
        "exactly one unit (the clean one) approves"
    );
    assert_eq!(
        result.rejected, 1,
        "exactly one unit (the secret one) is rejected"
    );

    let unit1 = &result.units[0];
    let unit2 = &result.units[1];

    // ── (a) unit-1: the governance DENY fired THROUGH the agent → phase Rejected, NOT approved. ──
    assert_eq!(
        unit1.decision.as_deref(),
        Some("deny"),
        "unit-1's context embeds the secret ⇒ governance decides Deny"
    );
    assert_eq!(
        unit1.phase_status, "rejected",
        "the deny drove the orchestration phase to rejected THROUGH the agent (ADR-0003)"
    );
    assert!(!unit1.approved, "a denied unit is NEVER approved");

    // ── (b) unit-2: clean context ⇒ Approved, with a work-output node recorded. ──
    assert_eq!(
        unit2.decision.as_deref(),
        Some("allow"),
        "unit-2's context is clean ⇒ governance decides Allow"
    );
    assert_eq!(
        unit2.phase_status, "approved",
        "the clean unit's phase is approved"
    );
    assert!(unit2.approved);

    // ── (c) EVERYTHING persists on the SAME store — read back via get_node / find_symbols. ──

    // The session node.
    let session = get_session(&store, "sess-e2e")
        .expect("read session")
        .expect("session node persists on the shared store");
    assert_eq!(session.status, SessionStatus::Completed);
    assert_eq!(session.id, "sess-e2e");

    // Both work-unit nodes (with their assignment + final status).
    let units = session_units(&store, "sess-e2e").expect("read units");
    assert_eq!(
        units.len(),
        2,
        "both work-unit nodes persist on the shared store"
    );
    assert_eq!(units[0].status, UnitStatus::Rejected);
    assert_eq!(units[1].status, UnitStatus::Done);
    assert_eq!(
        units[0].assigned_cli.as_deref(),
        Some("fake-a"),
        "the council assignment was recorded on the shared unit node"
    );
    assert!(
        units[0].conformance_ref.is_some() && units[1].conformance_ref.is_some(),
        "each unit node carries its ConformanceClaim id"
    );

    // The ConformanceClaim node(s) — at minimum the deny claim (the load-bearing one) persists.
    let deny_claim_id = unit1.claim_id.as_deref().expect("unit-1 minted a claim");
    let claim_node = store
        .get_node(&synthetic_symbol(CONFORMANCE_CLAIM, deny_claim_id))
        .expect("get_node ok")
        .expect("the deny ConformanceClaim node persists on the shared store");
    assert!(matches!(
        &claim_node.kind,
        NodeKind::Other(k) if k == CONFORMANCE_CLAIM
    ));
    // The recorded claim is a Deny (the durable evidence of WHY unit-1 was rejected).
    let recovered_claim = wicked_governance::claim_from_node(&claim_node).expect("decode claim");
    assert_eq!(recovered_claim.decision, wicked_apps_core::Decision::Deny);
    assert!(
        recovered_claim
            .policy_ids
            .contains(&"pol-deny-secrets".to_string()),
        "the deny policy participated in the claim"
    );

    // Both phase nodes persist, with the correct resolved status.
    let phase1 = wicked_orchestration::get_phase(&store, &unit1.phase_id)
        .expect("get phase 1")
        .expect("unit-1 phase node persists");
    let phase2 = wicked_orchestration::get_phase(&store, &unit2.phase_id)
        .expect("get phase 2")
        .expect("unit-2 phase node persists");
    assert_eq!(
        phase1.status,
        wicked_orchestration::PhaseStatus::Rejected,
        "the persisted phase carries the rejected status"
    );
    assert_eq!(
        phase1.gate_decision,
        Some(wicked_apps_core::Decision::Deny),
        "the persisted phase carries the hard Deny veto marker (ADR-0003)"
    );
    assert_eq!(phase2.status, wicked_orchestration::PhaseStatus::Approved);

    // The work-output node exists ONLY for the approved unit.
    assert!(
        get_work_output(&store, &unit1.unit_id).unwrap().is_none(),
        "the REJECTED unit recorded NO work-output node"
    );
    let out2 = get_work_output(&store, &unit2.unit_id)
        .unwrap()
        .expect("the APPROVED unit recorded a work-output node on the shared store");
    assert!(matches!(&out2.kind, NodeKind::Other(k) if k == WORK_OUTPUT));

    // Belt-and-braces: the four node kinds are all queryable on the ONE store via find_symbols.
    for (kind, at_least) in [
        (AGENT_SESSION, 1),
        (WORK_UNIT, 2),
        (PHASE, 2),
        (CONFORMANCE_CLAIM, 1),
    ] {
        let q = wicked_estate_core::SymbolQuery {
            kinds: vec![NodeKind::Other(kind.to_string())],
            ..Default::default()
        };
        let found = store.find_symbols(&q).expect("find_symbols ok");
        assert!(
            found.len() >= at_least,
            "expected ≥{at_least} {kind} node(s) on the shared store, found {}",
            found.len()
        );
    }

    clear_emit(&spool);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shared vs isolated is a collection-scope decision on the SAME store: shared pins all units'
/// outputs to ONE scope; isolated gives each unit its own. Both run on one in-memory store.
#[cfg(unix)]
#[test]
fn shared_vs_isolated_scope_on_the_same_store() {
    let dir = unique_tempdir("scope");
    let spool = hermetic_emit();

    let a = write_fake_cli(&dir, "fa.sh", "fake-a");
    let roster = vec![fake_cli_record("fake-a", &a)];
    // A clean 2-unit problem (no policy registered ⇒ both approve, both write outputs).
    let problem = "Build the api. Build the ui.";

    // SHARED: both units' outputs share ONE scope.
    let mut shared_store = SqliteStore::in_memory().unwrap();
    let shared = run_session(
        &mut shared_store,
        roster.clone(),
        problem,
        EntityMode::Shared,
        Some("sess-shared"),
    )
    .expect("shared session");
    assert_eq!(shared.approved, 2, "no deny policy ⇒ both units approve");
    assert_eq!(
        shared.units[0].collection_scope, shared.units[1].collection_scope,
        "shared mode: every unit's output is the SAME collection scope (one entity)"
    );
    assert_eq!(
        shared.collection_scope.as_deref(),
        Some("wicked-agent/sess-shared/shared")
    );

    // ISOLATED: each unit gets its own scope, on a fresh shared store.
    let mut iso_store = SqliteStore::in_memory().unwrap();
    let isolated = run_session(
        &mut iso_store,
        roster,
        problem,
        EntityMode::Isolated,
        Some("sess-iso"),
    )
    .expect("isolated session");
    assert_eq!(isolated.approved, 2);
    assert_ne!(
        isolated.units[0].collection_scope, isolated.units[1].collection_scope,
        "isolated mode: each unit gets its OWN collection scope on the same store"
    );
    assert!(
        isolated.collection_scope.is_none(),
        "isolated has no single session scope"
    );

    clear_emit(&spool);
    let _ = std::fs::remove_dir_all(&dir);
}
