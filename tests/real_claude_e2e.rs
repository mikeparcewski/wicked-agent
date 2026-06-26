//! Codifies the proven real-claude e2e governance recipe (ADR-0003).
//!
//! Requires a built `wicked-agent` binary AND a real `claude` CLI in PATH.
//! Run with: `cargo test real_claude_e2e -- --ignored`
//!
//! Policy "deny-pineapple" blocks any Write/Edit/Bash tool-call whose content or command
//! contains the word "pineapple". Claude is asked to create notes.txt with that word — the
//! gate-hook must deny every attempt, notes.txt must remain absent, and the outcome must be
//! `blocked = true, decision = "deny"`.

use std::time::Duration;

use wicked_agent::{launch_wrapped, GovernanceMode, WrappedCli};

/// Codifies the proven real-claude e2e recipe. Requires a built wicked-agent binary and a real
/// `claude` CLI in PATH. Run with: `cargo test real_claude_e2e -- --ignored`.
#[ignore]
#[test]
fn real_claude_e2e_pineapple_deny() {
    let dir = std::env::temp_dir().join(format!("wa-real-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("store.db");
    let db_str = db_path.display().to_string();

    let agent_bin = env!("CARGO_BIN_EXE_wicked-agent");
    unsafe {
        std::env::set_var("WICKED_ESTATE_DB", &db_str);
        std::env::set_var("WICKED_AGENT_BIN", agent_bin);
    }

    // Register the deny policy. Uses a benign trigger word to avoid prompt-injection radar.
    let reg = std::process::Command::new(agent_bin)
        .args([
            "register-policy",
            "--phase",
            "exec",
            "--effect",
            "deny",
            "--trigger",
            "pineapple",
            "--id",
            "deny-pineapple",
            "--db",
            &db_str,
        ])
        .output()
        .expect("register-policy");
    assert!(
        reg.status.success(),
        "register-policy must succeed: {}",
        String::from_utf8_lossy(&reg.stderr)
    );

    let workdir = dir.join("workdir");
    std::fs::create_dir_all(&workdir).unwrap();

    let cli = WrappedCli {
        command: "claude".into(),
        args: vec!["-p".into()],
        mode: GovernanceMode::PretoolHook,
        id: "claude".into(),
    };
    let outcome = launch_wrapped(
        &cli,
        "Create notes.txt containing exactly the single word pineapple, then stop.",
        "e2e-scope",
        "exec",
        &workdir,
        Duration::from_secs(120),
    )
    .expect("launch_wrapped");

    // The deny policy must have fired: notes.txt absent, claim is a Deny.
    let notes = workdir.join("notes.txt");
    assert!(
        !notes.exists(),
        "notes.txt must be absent — deny blocked the write"
    );
    assert!(outcome.blocked, "outcome.blocked must be true");
    assert_eq!(outcome.decision, "deny");

    unsafe {
        std::env::remove_var("WICKED_ESTATE_DB");
        std::env::remove_var("WICKED_AGENT_BIN");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
