//! INJECT — FORCING RIGOR on a REAL wrapped CLI subprocess (ARCHITECTURE §5; ADR-0003).
//!
//! The Rust port of the Node prototype's `lib/inject.mjs` (`launchWrapped`). The R5 execute step
//! used a deterministic stub for the unit's work; R6 LAUNCHES the assigned CLI as a REAL subprocess
//! (`std::process::Command`) in a sandbox workdir to perform the unit's task — GOVERNED, GATED, and
//! EVIDENCED. Two real gate mechanisms, both of which guarantee a denied effect NEVER lands:
//!
//!   · **per-tool-call PRE-HOOK** (finer-grained): wicked-agent drops a tiny governance hook into
//!     the CLI's sandbox. The wrapped CLI consults `$WICKED_PRETOOL_HOOK` with its PROPOSED tool-call
//!     on stdin BEFORE acting; the hook runs governance's deterministic `select`+`decide` over the
//!     proposed call and exits 2 on `Deny` (exit 0 on allow). On a non-zero exit the CLI ABORTS the
//!     action — the destructive effect never happens. Enforcement BEFORE the action. (Like inject.mjs.)
//!   · **post-hoc** (for incapable CLIs): the CLI runs free and surfaces its tool-calls; wicked-agent
//!     runs the SAME `select`+`decide` over each AFTER the fact. A `Deny` rejects the unit and the
//!     effect is rolled back from the sandbox. The gate still fires — later, weaker.
//!
//! THE INVARIANT (ADR-0003): the gate fires on EVERY launch. `blocked == true` means a tool-call was
//! denied — pretool aborted it before it ran, or post-hoc rejected the unit and rolled it back.
//! Either way the forbidden effect is not allowed to stand. We CORROBORATE the pretool hook against
//! the real subprocess: a denied tool-call's file MUST be absent on disk (the test asserts this).
//!
//! Reuse, don't re-invent: the decision engine is governance's (`select`+`decide`, NO model,
//! reproducible — the same engine the in-process unit gate uses). wicked-agent owns only the GLUE
//! that wires that engine onto a third-party subprocess across the process boundary.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use wicked_apps_core::{ConformanceClaim, Decision, GraphRead, GraphStore};
use serde::{Deserialize, Serialize};
use wicked_governance::{decide, select};

/// The governance mode (capability) of a wrapped CLI — its gate MECHANISM (ADR-0003 step 2/3).
/// The gate ALWAYS fires; only its TIMING degrades.
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

/// The wrapped-CLI descriptor for a real launch (the R6 path). The CLI is invoked as
/// `command [args...] <TASK.txt>`; it reads the task, performs its real action, and surfaces each
/// tool-call as a JSON line on stdout (see [`ToolCall`]). A `pretool-hook` CLI additionally consults
/// `$WICKED_PRETOOL_HOOK` BEFORE acting and aborts on a non-zero exit.
#[derive(Debug, Clone)]
pub struct WrappedCli {
    /// The executable (e.g. `"claude"`, `"/path/to/fake-agent.sh"`).
    pub command: String,
    /// Leading args before the task file.
    pub args: Vec<String>,
    /// The governance mechanism (capability).
    pub mode: GovernanceMode,
    /// A label for evidence/provenance (usually the council-assigned CLI key).
    pub id: String,
}

/// A proposed (or executed) tool-call the wrapped CLI surfaces on stdout, one JSON object per line,
/// shaped `{"tool_call": {"tool","command","path","content",...}}`. Mirrors the inject.mjs contract.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    /// The tool the CLI proposes to use (e.g. `"write_file"`, `"bash"`).
    #[serde(default)]
    pub tool: Option<String>,
    /// A shell command, if the tool is a command runner.
    #[serde(default)]
    pub command: Option<String>,
    /// The primary output path the call writes (resolved as the unit artifact).
    #[serde(default)]
    pub path: Option<String>,
    /// The content the call writes (a deny policy's `contains` matches over this).
    #[serde(default)]
    pub content: Option<String>,
    /// Free-form args.
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// The outcome of a real wrapped-CLI launch — the harness records this onto the unit + store.
#[derive(Debug, Clone, Serialize)]
pub struct LaunchOutcome {
    /// The tool-calls the CLI surfaced on stdout.
    pub tool_calls: Vec<ToolCall>,
    /// Did the gate BLOCK a tool-call (pretool aborted it / post-hoc rejected it)?
    pub blocked: bool,
    /// Why it was blocked, if so.
    pub blocked_reason: Option<String>,
    /// The authoritative governance decision the gate consumed (`allow`/`deny`/`allow_with_conditions`).
    pub decision: String,
    /// The real output artifact path the CLI declared (absolute), if any.
    pub artifact_path: Option<String>,
    /// The real artifact CONTENT captured from disk (None when blocked / no file).
    pub artifact: Option<String>,
    /// The gating ConformanceClaim (the durable evidence for the run).
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
    /// The sandbox workdir the CLI ran in (for inspection; caller-pinned dirs are theirs to clean).
    pub workdir: String,
}

/// Files/dirs wicked-agent INJECTS into the sandbox — preserved on a rollback so we quarantine ONLY
/// the CLI's own effects, never the harness's task order or governance hook.
const HARNESS_OWNED: &[&str] = &["TASK.txt", ".wicked-agent-hook", "prompt.txt"];

/// Launch a wrapped CLI as a REAL subprocess to perform a unit's task, GOVERNED + GATED + EVIDENCED.
///
/// `store` is the ONE shared estate store (the governance hook decides against the SAME policies the
/// in-process unit gate uses). `scope`/`phase` are the governance scope + phase the policies key on.
/// `workdir` is the sandbox the CLI performs its real work in (the caller pins it so artifacts are
/// inspectable). `unit_description` becomes the `TASK.txt` work order. `timeout` bounds the subprocess.
///
/// The gate's MECHANISM depends on `cli.mode`, but the gate ALWAYS fires and a denied effect is never
/// allowed to stand (pretool aborts before the write; post-hoc rolls the sandbox back).
pub fn launch_wrapped<S: GraphRead + GraphStore>(
    store: &mut S,
    cli: &WrappedCli,
    unit_description: &str,
    scope: &str,
    phase: &str,
    workdir: &Path,
    timeout: Duration,
) -> anyhow::Result<LaunchOutcome> {
    std::fs::create_dir_all(workdir)
        .map_err(|e| anyhow::anyhow!("create sandbox workdir {}: {e}", workdir.display()))?;

    // ── Drop the TASK the CLI reads (its real work order). ──
    let task_file = workdir.join("TASK.txt");
    std::fs::write(&task_file, unit_description)
        .map_err(|e| anyhow::anyhow!("write task file: {e}"))?;

    // ── Materialize the PreToolUse hook (always written; only CONSULTED by pretool-hook CLIs). ──
    // The hook is a self-contained POSIX shell script that re-invokes THIS binary's `gate-hook`
    // subcommand, so the subprocess decides EXACTLY like the in-process engine (same select+decide
    // over the SAME on-disk store). It reads the proposed tool-call JSON on stdin and exits 2 on deny.
    let hook_dir = workdir.join(".wicked-agent-hook");
    let hook_path = write_pretool_hook(&hook_dir, scope, phase)?;

    // ── Run the real subprocess. ──
    let mut command = Command::new(&cli.command);
    command
        .args(&cli.args)
        .arg(&task_file)
        .current_dir(workdir)
        .env("WICKED_GOV_PHASE", phase)
        .env("WICKED_GOV_SCOPE", scope)
        .env("WICKED_GOV_MODE", cli.mode.timing())
        .env("WICKED_TASK_FILE", &task_file)
        .env("WICKED_WORKDIR", workdir)
        .env(
            "WICKED_PRETOOL_HOOK",
            if cli.mode == GovernanceMode::PretoolHook {
                hook_path.as_os_str().to_owned()
            } else {
                std::ffi::OsString::new()
            },
        )
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let (exit_code, stdout, stderr) = run_bounded(command, timeout)?;

    // ── Parse the tool-calls the CLI surfaced (one JSON object per line with a `tool_call` key). ──
    let tool_calls = parse_tool_calls(&stdout);

    // ── Determine the gate outcome by running governance over the proposed/executed tool-calls. ──
    // This is the AUTHORITATIVE claim (the pretool hook is the enforcement; this is the record).
    let mut blocked = false;
    let mut blocked_reason: Option<String> = None;
    let mut decision_token = "allow".to_string();
    let mut gating_claim: Option<ConformanceClaim> = None;

    for (i, call) in tool_calls.iter().enumerate() {
        let context = tool_call_context(call, phase, scope);
        let selected = select(store, scope, phase, &context)?;
        let claim = decide(&selected, scope, phase, &context, EVAL_AT_BASE + i as i64);
        decision_token = decision_str(&claim.decision).to_string();
        let denied = claim.decision == Decision::Deny;
        gating_claim = Some(claim);
        if denied {
            blocked = true;
            blocked_reason = Some(format!(
                "{} denied tool-call ({})",
                match cli.mode {
                    GovernanceMode::PretoolHook => "pretool hook",
                    GovernanceMode::PostHoc => "post-hoc evaluate",
                },
                call.tool.as_deref().or(call.command.as_deref()).unwrap_or("?")
            ));
            break;
        }
    }

    // The CLI's declared primary artifact (the first tool-call carrying a `path`).
    let artifact_path: Option<PathBuf> = tool_calls
        .iter()
        .find_map(|c| c.path.as_deref().filter(|p| !p.is_empty()))
        .map(|p| resolve_out_path(p, workdir));

    // ── Capture the artifact (allow) OR roll back the sandbox (deny). ──
    let artifact: Option<String> = if blocked {
        // The forbidden effect must NOT stand. Pretool aborted before the write (usually nothing to
        // undo); post-hoc already ran (roll it back). Either way we quarantine the CLI's on-disk
        // effects within the sandbox — the bounded blast radius (ADR-0003 §9.3).
        rollback_workdir(workdir, artifact_path.as_deref());
        None
    } else {
        // Approved: read the real artifact back from disk (the CLI's genuine output).
        artifact_path
            .as_deref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
    };

    Ok(LaunchOutcome {
        tool_calls,
        blocked,
        blocked_reason,
        decision: decision_token,
        artifact_path: artifact_path.map(|p| p.display().to_string()),
        artifact,
        claim: gating_claim,
        mode: cli.mode,
        gate_timing: cli.mode.timing().to_string(),
        stdout,
        stderr,
        exit_code,
        workdir: workdir.display().to_string(),
    })
}

/// A fixed evaluation-timestamp base for claims minted on the launch path (deterministic per
/// tool-call offset; no wall clock on the decision path). Matches `execute.rs`'s convention.
const EVAL_AT_BASE: i64 = 1_750_000_000;

/// Build the governance context for a single proposed tool-call (matches the hook EXACTLY so the
/// pretool decision and the authoritative decision agree). `work` mirrors the unit gate's context
/// key so the SAME policies fire either way.
fn tool_call_context(call: &ToolCall, phase: &str, scope: &str) -> serde_json::Value {
    let work = call
        .command
        .clone()
        .or_else(|| call.content.clone())
        .or_else(|| call.tool.clone())
        .unwrap_or_default();
    serde_json::json!({
        "phase": phase,
        "scope": scope,
        "tool": call.tool,
        "command": call.command,
        "path": call.path,
        "content": call.content,
        "args": call.args,
        "work": work,
    })
}

/// Parse the CLI's stdout into tool-calls: one JSON object per line with a `tool_call` key. Non-JSON
/// lines (human logs) are ignored — tolerant exactly like inject.mjs.
fn parse_tool_calls(stdout: &str) -> Vec<ToolCall> {
    #[derive(Deserialize)]
    struct Line {
        tool_call: ToolCall,
    }
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Line>(l).ok().map(|x| x.tool_call))
        .collect()
}

/// Resolve a CLI-declared output path against the sandbox workdir (absolute paths pass through).
fn resolve_out_path(p: &str, workdir: &Path) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    }
}

/// Roll back the CLI's on-disk effects within the sandbox after a deny (the bounded blast radius,
/// ADR-0003 §9.3). Removes the declared artifact AND any other entry the CLI produced in the
/// workdir, preserving ONLY the harness's injected files. A Bash side-effect (a file written by a
/// denied command) is undone even though the harness never knew its path. Best-effort.
fn rollback_workdir(workdir: &Path, artifact_path: Option<&Path>) {
    if let Some(p) = artifact_path {
        if p.exists() {
            let _ = std::fs::remove_file(p);
        }
    }
    let owned: BTreeSet<&str> = HARNESS_OWNED.iter().copied().collect();
    let Ok(entries) = std::fs::read_dir(workdir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if owned.contains(name_str.as_ref()) {
            continue; // never remove the task order or the governance hook
        }
        let path = entry.path();
        if path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Write the PreToolUse hook into `dir` as an executable POSIX shell script that re-invokes THIS
/// binary's `gate-hook` subcommand (resolved via `current_exe`). The hook reads the proposed
/// tool-call JSON on stdin and exits 2 on a `Deny`, 0 otherwise — so the subprocess gates across the
/// process boundary using the SAME engine + SAME on-disk store. Returns the hook path.
#[cfg(unix)]
fn write_pretool_hook(dir: &Path, scope: &str, phase: &str) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("mkdir hook dir: {e}"))?;
    let self_exe = gate_hook_exe()?;
    let db = std::env::var("WICKED_ESTATE_DB").unwrap_or_default();
    // The hook pipes its stdin (the proposed tool-call) into `wicked-agent gate-hook`, passing the
    // scope/phase/db so the subprocess decides against the same on-disk store. exit code is the gate.
    let hook_path = dir.join("pretool-governance-hook.sh");
    let script = format!(
        "#!/bin/sh\n\
         # wicked-agent PreToolUse hook (generated). Reads a proposed tool-call as JSON on stdin,\n\
         # asks governance via the SAME engine + on-disk store, exits 0=allow / 2=DENY.\n\
         exec \"{exe}\" gate-hook --scope \"{scope}\" --phase \"{phase}\" --db \"{db}\"\n",
        exe = self_exe.display(),
        scope = scope,
        phase = phase,
        db = db,
    );
    std::fs::write(&hook_path, script).map_err(|e| anyhow::anyhow!("write hook: {e}"))?;
    let mut perms = std::fs::metadata(&hook_path)
        .map_err(|e| anyhow::anyhow!("stat hook: {e}"))?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&hook_path, perms).map_err(|e| anyhow::anyhow!("chmod hook: {e}"))?;
    Ok(hook_path)
}

/// Resolve the executable the generated hook re-invokes for `gate-hook`. Prefers the explicit
/// `WICKED_AGENT_BIN` override (so an integration test — whose `current_exe` is the test harness,
/// not `wicked-agent` — can point the hook at the real built binary, and the demo can pin it), else
/// falls back to this process's own `current_exe` (the production path: the `wicked-agent` binary
/// generates a hook that re-invokes itself).
fn gate_hook_exe() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("WICKED_AGENT_BIN") {
        let p = PathBuf::from(p);
        if !p.as_os_str().is_empty() {
            return Ok(p);
        }
    }
    std::env::current_exe().map_err(|e| anyhow::anyhow!("resolve current_exe for the gate hook: {e}"))
}

#[cfg(not(unix))]
fn write_pretool_hook(dir: &Path, scope: &str, phase: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir).map_err(|e| anyhow::anyhow!("mkdir hook dir: {e}"))?;
    let self_exe = gate_hook_exe()?;
    let db = std::env::var("WICKED_ESTATE_DB").unwrap_or_default();
    let hook_path = dir.join("pretool-governance-hook.cmd");
    let script = format!(
        "@echo off\r\n\"{exe}\" gate-hook --scope \"{scope}\" --phase \"{phase}\" --db \"{db}\"\r\n",
        exe = self_exe.display(),
        scope = scope,
        phase = phase,
        db = db,
    );
    std::fs::write(&hook_path, script).map_err(|e| anyhow::anyhow!("write hook: {e}"))?;
    Ok(hook_path)
}

/// The `gate-hook` subcommand body: read a proposed tool-call JSON on stdin, open the on-disk store,
/// run governance `select`+`decide` over the call's context, print the decision JSON, and return the
/// gate exit code (2 = DENY ⇒ the CLI must abort; 0 = allow). Fails CLOSED: if governance cannot
/// decide, the gate DENIES (rigor is unavoidable). This is the in-subprocess half of the pre-hook.
pub fn run_gate_hook(scope: &str, phase: &str, db: Option<&str>) -> i32 {
    use std::io::Read;
    let mut raw = String::new();
    let _ = std::io::stdin().read_to_string(&mut raw);
    let call: ToolCall = serde_json::from_str(raw.trim()).unwrap_or_default();
    let context = tool_call_context(&call, phase, scope);

    // Open the SAME on-disk store the in-process engine wrote the policies to.
    let store = match wicked_apps_core::open_store(db.filter(|s| !s.is_empty())) {
        Ok(s) => s,
        Err(e) => {
            // Fail CLOSED.
            println!("{{\"decision\":\"deny\",\"reason\":\"open store failed: {e}\"}}");
            return 2;
        }
    };
    let selected = match select(&store, scope, phase, &context) {
        Ok(s) => s,
        Err(e) => {
            println!("{{\"decision\":\"deny\",\"reason\":\"select failed: {e}\"}}");
            return 2;
        }
    };
    let claim = decide(&selected, scope, phase, &context, EVAL_AT_BASE);
    let denied = claim.decision == Decision::Deny;
    println!(
        "{{\"decision\":\"{}\",\"claim_id\":\"{}\"}}",
        decision_str(&claim.decision),
        claim.claim_id
    );
    if denied {
        2
    } else {
        0
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

/// The snake_case decision token for an outcome / hook payload.
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
    fn parse_tool_calls_ignores_non_json_lines() {
        let stdout = "starting work\n\
            {\"tool_call\":{\"tool\":\"write_file\",\"path\":\"out.txt\",\"content\":\"hi\"}}\n\
            done.\n";
        let calls = parse_tool_calls(stdout);
        assert_eq!(calls.len(), 1, "exactly one JSON tool_call line parses");
        assert_eq!(calls[0].tool.as_deref(), Some("write_file"));
        assert_eq!(calls[0].path.as_deref(), Some("out.txt"));
        assert_eq!(calls[0].content.as_deref(), Some("hi"));
    }

    #[test]
    fn tool_call_context_mirrors_hook_keys() {
        let call = ToolCall {
            tool: Some("bash".into()),
            command: Some("echo hi > f".into()),
            path: Some("f".into()),
            content: None,
            args: None,
        };
        let ctx = tool_call_context(&call, "unit-1", "wicked-agent/s/shared");
        assert_eq!(ctx["phase"], "unit-1");
        assert_eq!(ctx["scope"], "wicked-agent/s/shared");
        assert_eq!(ctx["tool"], "bash");
        // `work` falls back to the command (so a deny policy keyed on the command text fires).
        assert_eq!(ctx["work"], "echo hi > f");
    }

    #[test]
    fn resolve_out_path_joins_relative_passes_absolute() {
        let wd = Path::new("/tmp/sandbox");
        assert_eq!(resolve_out_path("out.txt", wd), PathBuf::from("/tmp/sandbox/out.txt"));
        assert_eq!(resolve_out_path("/etc/passwd", wd), PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn governance_mode_timing_tokens() {
        assert_eq!(GovernanceMode::PretoolHook.timing(), "pretool");
        assert_eq!(GovernanceMode::PostHoc.timing(), "post-hoc");
    }
}
