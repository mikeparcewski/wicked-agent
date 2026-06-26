//! INJECT — FORCING RIGOR on a REAL wrapped CLI subprocess (ARCHITECTURE §5; ADR-0003).
//!
//! The harness launches the assigned CLI as a REAL subprocess (`std::process::Command`) in a sandbox
//! workdir to perform the unit's task — GOVERNED, GATED, and EVIDENCED. Governance fires through
//! Claude's REAL PreToolUse hook mechanism (`.claude/settings.json`): the harness writes the hook
//! config before launch, the gate-hook subcommand handles each PreToolUse event, and the decisions
//! are appended to a run-local `decisions.ndjson` for the harness to read back after the process
//! exits. Exit 2 = deny (Claude aborts the tool-call BEFORE it runs); exit 0 = allow.
//!
//! THE INVARIANT (ADR-0003): the gate fires on EVERY launch. `blocked == true` means a tool-call was
//! denied — the forbidden effect was aborted before it ran.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wicked_apps_core::{ConformanceClaim, Decision};
use wicked_governance::{conform, decide, select};

/// The governance mode (capability) of a wrapped CLI — its gate MECHANISM (ADR-0003 step 2/3).
/// The gate ALWAYS fires; only its TIMING degrades for incapable CLIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GovernanceMode {
    /// Capable CLI: consults the PreToolUse hook before each tool-call (enforcement BEFORE).
    #[default]
    PretoolHook,
    /// Incapable CLI: runs free; wicked-agent evaluates its tool-calls AFTER (enforcement AFTER).
    PostHoc,
}

impl GovernanceMode {
    /// The gate-timing token recorded on the outcome.
    pub fn timing(self) -> &'static str {
        match self {
            GovernanceMode::PretoolHook => "pretool",
            GovernanceMode::PostHoc => "post-hoc",
        }
    }
}

/// The wrapped-CLI descriptor for a real launch. For claude the invocation is
/// `claude -p <TASK> --settings <settings.json> --permission-mode acceptEdits`; the harness writes
/// the governance hook into the settings file and reads decisions from the run-local ndjson log.
#[derive(Debug, Clone)]
pub struct WrappedCli {
    /// The executable (e.g. `"claude"`, `"/path/to/cli"`).
    pub command: String,
    /// Leading args before the task (e.g. `["-p"]` for claude headless).
    pub args: Vec<String>,
    /// The governance mechanism (capability).
    pub mode: GovernanceMode,
    /// A label for evidence/provenance (usually the council-assigned CLI key).
    pub id: String,
}

/// The outcome of a real wrapped-CLI launch — the harness records this onto the unit + store.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchOutcome {
    /// Did the gate BLOCK a tool-call (pretool aborted it)?
    pub blocked: bool,
    /// Why it was blocked, if so.
    pub blocked_reason: Option<String>,
    /// The authoritative governance decision the gate consumed (`allow`/`deny`/`allow_with_conditions`).
    pub decision: String,
    /// The real on-disk artifact path the CLI declared (absolute), if any.
    pub artifact_path: Option<String>,
    /// The real artifact CONTENT captured from disk (None when blocked / no file).
    pub artifact: Option<String>,
    /// The gating ConformanceClaim (the durable evidence for the run), from decisions.ndjson.
    pub claim: Option<ConformanceClaim>,
    /// The gate mechanism used.
    pub mode: GovernanceMode,
    /// `pretool` | `post-hoc`.
    pub gate_timing: String,
    /// The subprocess's raw stdout (captured as evidence).
    pub stdout: String,
    /// The subprocess's raw stderr (captured as evidence).
    pub stderr: String,
    /// The subprocess's real exit code (`-1` if it could not be determined).
    pub exit_code: i32,
    /// The sandbox workdir the CLI ran in.
    pub workdir: String,
}

/// Launch a wrapped CLI as a REAL subprocess to perform a unit's task, GOVERNED + GATED + EVIDENCED.
///
/// Writes `.claude/settings.json` with a PreToolUse hook that calls `wicked-agent gate-hook`; the
/// hook fires for every Write/Edit/MultiEdit/Bash/NotebookEdit call Claude makes and appends each
/// `ConformanceClaim` to `.wicked-agent/decisions.ndjson` in the workdir. After the subprocess
/// exits, the harness reads that file: the first Deny is the authoritative blocking claim; if no
/// Deny, the last claim is used. `blocked` is true if any claim is a Deny.
pub fn launch_wrapped(
    cli: &WrappedCli,
    unit_description: &str,
    scope: &str,
    phase: &str,
    workdir: &Path,
    timeout: Duration,
) -> anyhow::Result<LaunchOutcome> {
    std::fs::create_dir_all(workdir)
        .map_err(|e| anyhow::anyhow!("create sandbox workdir {}: {e}", workdir.display()))?;

    // ── Write the Claude settings.json (the hook config the subprocess reads). ──
    let settings_path = write_claude_settings(workdir, scope, phase)?;

    // ── Run the real subprocess. ──
    let mut command = Command::new(&cli.command);
    command
        .args(&cli.args)
        .arg(unit_description)
        .arg("--settings")
        .arg(&settings_path)
        .arg("--permission-mode")
        .arg("acceptEdits")
        .current_dir(workdir)
        .env("WICKED_GOV_PHASE", phase)
        .env("WICKED_GOV_SCOPE", scope)
        .env("WICKED_GOV_MODE", cli.mode.timing())
        .env("WICKED_WORKDIR", workdir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let (exit_code, stdout, stderr) = run_bounded(command, timeout)?;

    // ── Read the gate decisions the hook appended during the run. ──
    let decisions_path = workdir.join(".wicked-agent").join("decisions.ndjson");
    let (gating_claim, blocked, blocked_reason, decision_token) =
        parse_decisions_file(&decisions_path);

    Ok(LaunchOutcome {
        blocked,
        blocked_reason,
        decision: decision_token,
        artifact_path: None,
        artifact: None,
        claim: gating_claim,
        mode: cli.mode,
        gate_timing: cli.mode.timing().to_string(),
        stdout,
        stderr,
        exit_code,
        workdir: workdir.display().to_string(),
    })
}

/// Parse the run-local `decisions.ndjson`: first Deny is authoritative (blocked); if no Deny, the
/// last claim is used. Returns `(claim, blocked, blocked_reason, decision_token)`.
pub fn parse_decisions_file(
    path: &Path,
) -> (Option<ConformanceClaim>, bool, Option<String>, String) {
    let mut gating_claim: Option<ConformanceClaim> = None;
    let mut blocked = false;
    let mut blocked_reason: Option<String> = None;
    let mut decision_token = "allow".to_string();

    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, false, None, decision_token);
    };

    for line in content.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(claim) = serde_json::from_str::<ConformanceClaim>(line) else {
            continue;
        };
        let denied = claim.decision == Decision::Deny;
        decision_token = decision_str(&claim.decision).to_string();
        if denied {
            blocked = true;
            blocked_reason = Some(format!("gate-hook denied (claim {})", claim.claim_id));
            gating_claim = Some(claim);
            break; // first Deny is authoritative
        } else {
            gating_claim = Some(claim); // last allow wins if no deny follows
        }
    }

    (gating_claim, blocked, blocked_reason, decision_token)
}

/// Write the governance PreToolUse hook into Claude's settings.json at `workdir/.claude/settings.json`.
/// The hook command re-invokes `wicked-agent gate-hook` for every tool-call Claude proposes; exit 2
/// = deny (Claude aborts); exit 0 = allow. Returns the absolute path to the settings file.
fn write_claude_settings(workdir: &Path, scope: &str, phase: &str) -> anyhow::Result<PathBuf> {
    let settings_dir = workdir.join(".claude");
    std::fs::create_dir_all(&settings_dir).map_err(|e| anyhow::anyhow!("mkdir .claude: {e}"))?;

    let self_exe = gate_hook_exe()?;
    let db = std::env::var("WICKED_ESTATE_DB").unwrap_or_default();
    let hook_cmd = format!(
        "{exe} gate-hook --scope {scope} --phase {phase} --db {db}",
        exe = self_exe.display(),
    );

    // Real Claude PreToolUse hook schema (verified against claude 2.1.191).
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Write|Edit|MultiEdit|Bash|NotebookEdit",
                "hooks": [{
                    "type": "command",
                    "command": hook_cmd
                }]
            }]
        }
    });

    let settings_path = settings_dir.join("settings.json");
    let json = serde_json::to_string_pretty(&settings)
        .map_err(|e| anyhow::anyhow!("serialize settings: {e}"))?;
    std::fs::write(&settings_path, json)
        .map_err(|e| anyhow::anyhow!("write settings.json: {e}"))?;
    Ok(settings_path)
}

/// Resolve the executable the hook re-invokes for `gate-hook`. Prefers the explicit
/// `WICKED_AGENT_BIN` override (so an integration test can point at the real built binary), else
/// falls back to this process's own `current_exe`.
fn gate_hook_exe() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("WICKED_AGENT_BIN") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("resolve current_exe for the gate hook: {e}"))
}

/// Map a tool-call event onto the governance evaluation context. Reads ONLY Claude's real
/// PreToolUse event shape `{ "tool_name", "tool_input": { … } }` (verified against claude 2.1.191).
/// `tool_input` keys vary by tool: `Bash{command}`, `Write{file_path,content}`,
/// `Edit{file_path,new_string}`, `Read{file_path}`, …. Returns the context plus the tool name.
fn claude_pretool_context(raw: &str, scope: &str, phase: &str) -> (serde_json::Value, String) {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).unwrap_or(serde_json::Value::Null);
    let tool = v
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let input = v
        .get("tool_input")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let get = |k: &str| {
        input
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let command = get("command");
    let path = get("file_path")
        .or_else(|| get("path"))
        .or_else(|| get("notebook_path"));
    let content = get("content")
        .or_else(|| get("new_string"))
        .or_else(|| get("new_str"));
    let work = command
        .clone()
        .or_else(|| content.clone())
        .or_else(|| path.clone())
        .unwrap_or_else(|| tool.clone());
    let context = serde_json::json!({
        "phase": phase,
        "scope": scope,
        "tool": tool,
        "command": command,
        "path": path,
        "content": content,
        "args": input,
        "work": work,
    });
    (context, tool)
}

/// A fixed evaluation-timestamp base for claims minted on the gate-hook path (deterministic; no
/// wall clock on the decision path). Matches `execute.rs`'s convention.
const EVAL_AT_BASE: i64 = 1_750_000_000;

/// The `gate-hook` subcommand body: the REAL Claude PreToolUse hook. Reads Claude's event JSON
/// (`{tool_name, tool_input, …}`) on stdin, opens the on-disk store, runs governance `select`+`decide`
/// over the call's context, **persists the `ConformanceClaim` as durable evidence**, **appends the
/// claim to `.wicked-agent/decisions.ndjson` (cwd-relative; cwd == workdir under launch)**, writes
/// any deny reason to stderr (Claude surfaces it to the model), and returns the gate exit code
/// (2 = DENY ⇒ Claude aborts the tool-call BEFORE it runs; 0 = allow). Fails CLOSED: if the store
/// cannot be opened or governance cannot decide, the gate DENIES. Prints nothing to stdout.
pub fn run_gate_hook(scope: &str, phase: &str, db: Option<&str>) -> i32 {
    use std::io::Read;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let (context, tool) = claude_pretool_context(&raw, scope, phase);

    let mut store = match wicked_apps_core::open_store(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (open store failed: {e})");
            return 2;
        }
    };
    let selected = match select(&store, scope, phase, &context) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-governance: DENY (policy select failed: {e})");
            return 2;
        }
    };
    let claim = decide(&selected, scope, phase, &context, EVAL_AT_BASE);

    // Append the claim to the run-local decisions log (cwd == workdir under launch).
    append_decision(&claim);

    // Record the decision on the shared store (best-effort; the verdict already stands).
    if let Err(e) = conform(&mut store, &claim) {
        eprintln!(
            "wicked-governance: NOTE — failed to persist claim {}: {e}",
            claim.claim_id
        );
    }

    match claim.decision {
        Decision::Deny => {
            let t = if tool.is_empty() {
                "tool-call"
            } else {
                tool.as_str()
            };
            eprintln!("wicked-governance: DENY `{t}` (claim {})", claim.claim_id);
            2
        }
        _ => 0,
    }
}

/// Append one serialized `ConformanceClaim` line to `.wicked-agent/decisions.ndjson` in cwd.
/// Best-effort — a failure to write is not a gate failure (the exit code is already decided).
fn append_decision(claim: &ConformanceClaim) {
    use std::io::Write;
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let dir = cwd.join(".wicked-agent");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("decisions.ndjson");
    if let Ok(json) = serde_json::to_string(claim) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(f, "{json}");
        }
    }
}

/// Run `command` to completion bounded by `timeout`, returning `(exit_code, stdout, stderr)`. Uses a
/// std-only watcher loop (no extra deps), mirroring the council dispatcher's bounded-wait.
fn run_bounded(mut command: Command, timeout: Duration) -> anyhow::Result<(i32, String, String)> {
    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn wrapped CLI: {e}"))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok((-1, String::new(), "timed out".to_string()));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(anyhow::anyhow!("wait on wrapped CLI: {e}")),
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("collect wrapped CLI output: {e}"))?;
    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    Ok((exit_code, stdout, stderr))
}

/// The snake_case decision token for an outcome.
fn decision_str(decision: &Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::AllowWithConditions => "allow_with_conditions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn governance_mode_timing_tokens() {
        assert_eq!(GovernanceMode::PretoolHook.timing(), "pretool");
        assert_eq!(GovernanceMode::PostHoc.timing(), "post-hoc");
    }

    #[test]
    fn write_claude_settings_produces_valid_settings_json() {
        let dir = std::env::temp_dir().join(format!("wa-settings-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("WICKED_AGENT_BIN", "/usr/bin/wicked-agent-test") };

        let settings_path = write_claude_settings(&dir, "test-scope", "exec").unwrap();

        assert!(settings_path.exists(), "settings.json must exist");
        let content = std::fs::read_to_string(&settings_path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");

        // Must have the nested PreToolUse hook structure.
        let hooks = &v["hooks"]["PreToolUse"];
        assert!(hooks.is_array() && !hooks.as_array().unwrap().is_empty());
        let entry = &hooks[0];
        assert_eq!(entry["matcher"], "Write|Edit|MultiEdit|Bash|NotebookEdit");
        let inner = &entry["hooks"][0];
        assert_eq!(inner["type"], "command");
        let cmd = inner["command"].as_str().unwrap();
        assert!(cmd.contains("gate-hook"), "command must invoke gate-hook");
        assert!(
            cmd.contains("--scope test-scope"),
            "command must pass scope"
        );
        assert!(cmd.contains("--phase exec"), "command must pass phase");

        unsafe { std::env::remove_var("WICKED_AGENT_BIN") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_decisions_file_first_deny_wins() {
        let dir = std::env::temp_dir().join(format!("wa-dec-deny-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.ndjson");

        let allow_claim = ConformanceClaim {
            claim_id: "claim-allow".into(),
            scope: "s".into(),
            phase: "p".into(),
            decision: Decision::Allow,
            policy_ids: vec![],
            evaluated_context_ref: String::new(),
            criteria: String::new(),
            evaluator_identity: String::new(),
            evaluated_at: 0,
            obligations: vec![],
        };
        let deny_claim = ConformanceClaim {
            claim_id: "claim-deny".into(),
            decision: Decision::Deny,
            ..allow_claim.clone()
        };
        let allow_claim2 = ConformanceClaim {
            claim_id: "claim-allow2".into(),
            ..allow_claim.clone()
        };

        use std::io::Write;
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&allow_claim).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&deny_claim).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&allow_claim2).unwrap()).unwrap();

        let (claim, blocked, reason, token) = parse_decisions_file(&path);
        assert!(blocked, "a deny line must set blocked");
        assert_eq!(claim.unwrap().claim_id, "claim-deny", "first deny wins");
        assert_eq!(token, "deny");
        assert!(reason.is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_decisions_file_no_deny_takes_last() {
        let dir = std::env::temp_dir().join(format!("wa-dec-allow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("decisions.ndjson");

        let make = |id: &str| ConformanceClaim {
            claim_id: id.into(),
            scope: "s".into(),
            phase: "p".into(),
            decision: Decision::Allow,
            policy_ids: vec![],
            evaluated_context_ref: String::new(),
            criteria: String::new(),
            evaluator_identity: String::new(),
            evaluated_at: 0,
            obligations: vec![],
        };

        use std::io::Write;
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", serde_json::to_string(&make("c1")).unwrap()).unwrap();
        writeln!(f, "{}", serde_json::to_string(&make("c2")).unwrap()).unwrap();

        let (claim, blocked, _reason, token) = parse_decisions_file(&path);
        assert!(!blocked);
        assert_eq!(
            claim.unwrap().claim_id,
            "c2",
            "last allow wins when no deny"
        );
        assert_eq!(token, "allow");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_decisions_file_absent_returns_defaults() {
        let path = std::path::Path::new("/tmp/wicked-agent-nonexistent-decisions.ndjson");
        let (claim, blocked, reason, token) = parse_decisions_file(path);
        assert!(claim.is_none());
        assert!(!blocked);
        assert!(reason.is_none());
        assert_eq!(token, "allow");
    }

    #[test]
    fn claude_pretool_context_write_extracts_path_and_content() {
        let raw = r#"{"tool_name":"Write","tool_input":{"file_path":"out.txt","content":"hello"}}"#;
        let (ctx, tool) = claude_pretool_context(raw, "scope", "exec");
        assert_eq!(tool, "Write");
        assert_eq!(ctx["tool"], "Write");
        assert_eq!(ctx["path"], "out.txt");
        assert_eq!(ctx["content"], "hello");
        // `work` falls back to content when no command.
        assert_eq!(ctx["work"], "hello");
    }

    #[test]
    fn claude_pretool_context_bash_extracts_command() {
        let raw = r#"{"tool_name":"Bash","tool_input":{"command":"echo hi"}}"#;
        let (ctx, tool) = claude_pretool_context(raw, "scope", "exec");
        assert_eq!(tool, "Bash");
        assert_eq!(ctx["command"], "echo hi");
        assert_eq!(ctx["work"], "echo hi");
    }

}
