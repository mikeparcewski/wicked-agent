//! Live INTEGRATED e2e (ADR-0003 + human-confirm gate + MCP toolbox): proves that ONE governed
//! session drives a REAL `claude` subprocess through plan → distribute → execute, with the wicked-*
//! MCP toolbox injected (augment mode) and a TYPED human-confirm gate that pauses the run and a
//! `resume` that completes it.
//!
//! Requires a real `claude` CLI in PATH AND the wicked-* MCP server binaries installed
//! (`wicked-estate-mcp` / `wicked-memory-mcp` / `wicked-knowledge-mcp`, e.g. in `~/.cargo/bin`).
//! Run with: `cargo test --test real_claude_human_confirm_e2e -- --ignored`
//!
//! What it proves, in ONE live session:
//! 1. GOVERNANCE wired — every unit workdir gets `.claude/settings.json` (the PreToolUse hook).
//! 2. TOOLBOX injected — every claude workdir gets `.claude/mcp.json` naming the wicked-* servers (augment mode `--mcp-config`, claude-family only).
//! 3. HUMAN-CONFIRM — `Before(2)` executes unit 1, then PAUSES (paused_at == 2, AwaitingHuman) before unit 2; no second claude launch.
//! 4. REAL WORK — claude actually creates the unit-1 artifact under acceptEdits.
//! 5. RESUME — `resume_session` runs unit 2 to completion (session Completed); claude creates the unit-2 artifact. No re-pause.

use std::time::Duration;

use wicked_agent::{
    discover_toolbox, get_session, resume_session, run_session_wrapped, scope::EntityMode,
    GovernanceMode, HumanConfirm, SessionStatus,
};
use wicked_apps_core::open_store;
use wicked_council::types::{AgenticCli, Category, Confidence, InputMode};

/// A single real-`claude` roster seat. The harness tokenizes `headless_invocation`, drops the
/// `{PROMPT}` placeholder, and launches `claude -p <task> --settings … --permission-mode acceptEdits
/// [--mcp-config …]` in the per-unit sandbox workdir.
fn real_claude_seat() -> AgenticCli {
    AgenticCli {
        key: "claude".into(),
        display_name: "Claude".into(),
        binary: "claude".into(),
        headless_invocation: "claude -p \"{PROMPT}\"".into(),
        category: Category::AgenticCoder,
        input_mode: InputMode::PromptArg,
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Confidence::Verified,
        enabled_for_council: true,
    }
}

#[ignore]
#[test]
fn real_claude_human_confirm_resume_with_toolbox() {
    let dir = std::env::temp_dir().join(format!("wa-hc-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_str = dir.join("store.db").display().to_string();

    let agent_bin = env!("CARGO_BIN_EXE_wicked-agent");
    unsafe {
        // The gate-hook child resolves the SAME store; the harness resolves the gate-hook binary.
        std::env::set_var("WICKED_ESTATE_DB", &db_str);
        std::env::set_var("WICKED_AGENT_BIN", agent_bin);
    }

    // PRECONDITION: the toolbox must actually resolve real MCP servers, else the injection proof is
    // vacuous. This is the runtime half of WS2's discover_toolbox coverage — here against the real bins.
    let toolbox = discover_toolbox();
    assert!(
        !toolbox.is_empty(),
        "discover_toolbox found no wicked-* MCP servers — install them (cargo install) so the \
         augment-mode injection proof is real; got {toolbox:?}"
    );
    eprintln!(
        "toolbox resolved {} server(s): {:?}",
        toolbox.len(),
        toolbox.iter().map(|s| &s.name).collect::<Vec<_>>()
    );

    let sandbox = dir.join("sandbox");
    let session_id = "hc-e2e";
    // TWO units, split ONLY on ';'. plan_units cuts on '.'/'!'/'?' ONLY when followed by whitespace,
    // so "step1.txt" (period followed by a letter) stays whole — exactly two units result. No deny
    // policy is registered, so the governance gate ALLOWS — claude does the work under acceptEdits.
    let problem = "Create a file named step1.txt containing exactly the single word alpha; \
                   Create a file named step2.txt containing exactly the single word beta";

    let mut store = open_store(Some(&db_str)).expect("open shared store");

    // ── PHASE 1: run with Before(2) → unit 1 executes, then PAUSE before unit 2. ──
    let paused = run_session_wrapped(
        &mut store,
        vec![real_claude_seat()],
        problem,
        EntityMode::Shared,
        Some(session_id),
        GovernanceMode::PretoolHook,
        &sandbox,
        Duration::from_secs(180),
        HumanConfirm::Before(2),
    )
    .expect("run_session_wrapped (phase 1)");

    assert_eq!(paused.paused_at, Some(2), "must pause BEFORE unit 2");
    assert_eq!(
        paused.units.len(),
        1,
        "exactly one unit (the first) executes before the pause"
    );
    let s1 = get_session(&store, session_id)
        .expect("read session")
        .expect("session exists");
    assert_eq!(
        s1.status,
        SessionStatus::AwaitingHuman,
        "paused session must be AwaitingHuman"
    );

    // ── GOVERNANCE + TOOLBOX injection for the REAL claude launch (unit 1 workdir). ──
    let u1 = &paused.units[0];
    assert_eq!(u1.ord, 1, "the executed unit is unit 1");
    let u1_workdir = sandbox.join(session_id).join(&u1.unit_id);

    let settings_json = u1_workdir.join(".claude").join("settings.json");
    assert!(
        settings_json.exists(),
        "governance PreToolUse hook (.claude/settings.json) must be wired at {}",
        settings_json.display()
    );

    let mcp_json = u1_workdir.join(".claude").join("mcp.json");
    assert!(
        mcp_json.exists(),
        "MCP toolbox (.claude/mcp.json) must be injected for the real claude launch at {}",
        mcp_json.display()
    );
    let mcp_v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_json).unwrap())
            .expect("mcp.json must be valid JSON");
    let servers = &mcp_v["mcpServers"];
    assert!(
        servers["wicked-estate"].is_object()
            || servers["wicked-memory"].is_object()
            || servers["wicked-knowledge"].is_object(),
        "mcp.json must name at least one wicked-* toolbox server (augment mode): {mcp_v}"
    );

    // ── REAL WORK: claude created the unit-1 artifact and the unit was approved (allow path). ──
    assert!(
        u1.approved,
        "unit 1 must be approved on the no-policy allow path (decision={:?}, blocked={})",
        u1.decision, u1.gate_blocked
    );
    let step1 = u1_workdir.join("step1.txt");
    assert!(
        step1.exists(),
        "claude must have created step1.txt in the unit-1 workdir {}",
        u1_workdir.display()
    );

    // Unit 2 must NOT have run yet — no workdir, no artifact.
    // (Its outcome is absent from the phase-1 result; proven by units.len()==1 above.)

    // ── PHASE 2: resume → unit 2 executes to completion, no re-pause. ──
    let done = resume_session(&mut store, session_id).expect("resume_session (phase 2)");
    assert_eq!(
        done.paused_at, None,
        "resume must drive the session to completion without re-pausing"
    );
    let s2 = get_session(&store, session_id)
        .expect("read session")
        .expect("session exists");
    assert_eq!(
        s2.status,
        SessionStatus::Completed,
        "resumed session must be Completed"
    );

    let u2 = done
        .units
        .iter()
        .find(|u| u.ord == 2)
        .expect("unit 2 outcome present after resume");
    assert!(u2.approved, "unit 2 must be approved on the allow path");
    let u2_workdir = sandbox.join(session_id).join(&u2.unit_id);
    let step2 = u2_workdir.join("step2.txt");
    assert!(
        step2.exists(),
        "claude must have created step2.txt on resume in the unit-2 workdir {}",
        u2_workdir.display()
    );

    unsafe {
        std::env::remove_var("WICKED_ESTATE_DB");
        std::env::remove_var("WICKED_AGENT_BIN");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
