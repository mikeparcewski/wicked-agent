//! INJECT — FORCING RIGOR on a REAL wrapped CLI subprocess (ARCHITECTURE §5; ADR-0003).
//!
//! The harness launches the assigned CLI as a REAL subprocess (`std::process::Command`) in a sandbox
//! workdir to perform the unit's task — GOVERNED, GATED, and EVIDENCED. Governance fires through
//! Claude's REAL PreToolUse hook mechanism (`.claude/settings.json`): the harness writes the hook
//! config before launch, the gate-hook subcommand handles each PreToolUse event, and the decisions
//! are appended to a run-local `decisions.ndjson` for the harness to read back after the process
//! exits. Exit 2 = deny (Claude aborts the tool-call BEFORE it runs); exit 0 = allow.
//!
//! MCP TOOLBOX INJECTION (augment mode):
//! For `claude` launches, the harness also writes a `mcpServers` config to
//! `workdir/.claude/mcp.json` and passes `--mcp-config <path>` (NOT `--strict-mcp-config`).
//! This is ADDITIVE: the launched Claude gets the user's own MCP servers PLUS the collection's
//! wicked-* toolbox servers. The toolbox is the 9 Rust crates: wicked-estate-mcp,
//! wicked-memory-mcp, wicked-knowledge-mcp, and future overlay/governance/orchestration/
//! council/agent/apps-core servers. Non-claude CLIs (agy, pi) are NOT given `--mcp-config`
//! and remain ungoverned at the per-tool-call level.
//!
//! THE INVARIANT (ADR-0003): the gate fires on EVERY launch. `blocked == true` means a tool-call was
//! denied — the forbidden effect was aborted before it ran.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use wicked_apps_core::{ConformanceClaim, Decision};
use wicked_governance::{conform, decide, select};

// ─────────────────────────────────────────────────────────────────────────────
// MCP toolbox injection — augment mode (NOT hermetic).
// ─────────────────────────────────────────────────────────────────────────────

/// One MCP server entry in the toolbox — the minimum fields Claude's `mcpServers` schema requires.
/// The `command` + `args` form the actual process launch; `env` is merged into the child environment.
/// Follows the Claude `--mcp-config` JSON schema: `{ "mcpServers": { "<name>": { ... } } }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerSpec {
    /// The logical name under `mcpServers` (e.g. `"wicked-estate"`, `"wicked-memory"`).
    pub name: String,
    /// The executable to launch (e.g. `"wicked-estate-mcp"` or an absolute path).
    pub command: String,
    /// Leading args passed to the MCP server process (often empty for stdio servers).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Optional environment variables merged into the MCP server's environment.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
}

/// Write the MCP server toolbox config to `workdir/.claude/mcp.json`.
///
/// The written JSON follows Claude's `--mcp-config` schema:
/// ```json
/// { "mcpServers": { "<name>": { "command": "...", "args": [...], "env": {...} } } }
/// ```
/// Returns `Some(path)` when the config was written (caller adds `--mcp-config <path>`), or `None`
/// when `toolbox` is empty (no flag added — matches the "no servers yet" infrastructure stub).
///
/// **Augment mode** (ADR-0003 extension): this config is always passed with `--mcp-config`, NEVER
/// `--strict-mcp-config`, so the launched CLI receives the user's existing MCP servers PLUS these.
pub fn write_mcp_config(
    workdir: &Path,
    toolbox: &[McpServerSpec],
) -> anyhow::Result<Option<PathBuf>> {
    if toolbox.is_empty() {
        return Ok(None);
    }

    let settings_dir = workdir.join(".claude");
    std::fs::create_dir_all(&settings_dir)
        .map_err(|e| anyhow::anyhow!("mkdir .claude for mcp.json: {e}"))?;

    // Build the `mcpServers` map from the toolbox specs.
    let mut servers = serde_json::Map::new();
    for spec in toolbox {
        let mut entry = serde_json::Map::new();
        entry.insert(
            "command".to_string(),
            serde_json::Value::String(spec.command.clone()),
        );
        if !spec.args.is_empty() {
            entry.insert(
                "args".to_string(),
                serde_json::Value::Array(
                    spec.args
                        .iter()
                        .map(|a| serde_json::Value::String(a.clone()))
                        .collect(),
                ),
            );
        }
        if !spec.env.is_empty() {
            let env_map: serde_json::Map<String, serde_json::Value> = spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect();
            entry.insert("env".to_string(), serde_json::Value::Object(env_map));
        }
        servers.insert(spec.name.clone(), serde_json::Value::Object(entry));
    }

    let config = serde_json::json!({ "mcpServers": servers });
    let mcp_path = settings_dir.join("mcp.json");
    let json = serde_json::to_string_pretty(&config)
        .map_err(|e| anyhow::anyhow!("serialize mcp.json: {e}"))?;
    std::fs::write(&mcp_path, json).map_err(|e| anyhow::anyhow!("write mcp.json: {e}"))?;
    Ok(Some(mcp_path))
}

/// Discover the toolbox `McpServerSpec` list by probing known wicked-* MCP server binaries.
///
/// Resolution order per server:
///   1. An explicit env var override, e.g. `WICKED_ESTATE_MCP_BIN=/path/to/wicked-estate-mcp`.
///   2. The binary found on `$PATH` via a cheap `which`-style probe.
///   3. The Cargo home (`~/.cargo/bin/<name>`) as a last resort (installed via `cargo install`).
///
/// When NONE of the probe steps finds a binary, that server is silently omitted — the harness
/// degrades gracefully (fewer toolbox tools, not a hard error). This lets the infrastructure stub
/// (`write_mcp_config` with an empty slice) coexist with a partially-built toolbox.
///
/// Toolbox = 9 crates (wicked-apps-core is a library-only crate, so only 3 expose MCP servers
/// today; the remaining 6 are registered as future slots once they ship MCP server binaries):
///   - wicked-estate-mcp  (wicked-estate)   — code graph / semantic search
///   - wicked-memory-mcp  (wicked-memory)   — memory capture / recall
///   - wicked-knowledge-mcp (wicked-knowledge) — knowledge graph
///   - wicked-overlay-mcp (wicked-overlay)  — cross-store edges (future)
///   - wicked-governance-mcp (wicked-governance) — policy query (future)
///   - wicked-orchestration-mcp (wicked-orchestration) — workflow query (future)
///   - wicked-council-mcp (wicked-council)  — CLI roster / distribution (future)
///   - wicked-agent-mcp (wicked-agent)      — session / unit query (future)
pub fn discover_toolbox() -> Vec<McpServerSpec> {
    /// Known MCP server entries: (binary name, logical server name, env-override var).
    const KNOWN: &[(&str, &str, &str)] = &[
        (
            "wicked-estate-mcp",
            "wicked-estate",
            "WICKED_ESTATE_MCP_BIN",
        ),
        (
            "wicked-memory-mcp",
            "wicked-memory",
            "WICKED_MEMORY_MCP_BIN",
        ),
        (
            "wicked-knowledge-mcp",
            "wicked-knowledge",
            "WICKED_KNOWLEDGE_MCP_BIN",
        ),
    ];

    let mut specs = Vec::new();

    for &(binary, name, env_var) in KNOWN {
        if let Some(cmd) = resolve_mcp_binary(binary, env_var) {
            specs.push(McpServerSpec {
                name: name.to_string(),
                command: cmd,
                args: Vec::new(),
                env: Default::default(),
            });
        }
    }

    specs
}

/// Resolve one MCP server binary: env override → PATH probe → Cargo home fallback.
/// Returns `None` when the binary is not found by any strategy.
fn resolve_mcp_binary(binary: &str, env_var: &str) -> Option<String> {
    // 1. Explicit env override (highest priority — lets CI/tests pin exact paths).
    if let Ok(p) = std::env::var(env_var) {
        if !p.is_empty() && std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }

    // 2. PATH probe — use `which`-style search through PATH entries.
    if let Ok(path_val) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_val) {
            let candidate = dir.join(binary);
            if candidate.exists() {
                return Some(candidate.display().to_string());
            }
        }
    }

    // 3. Cargo home fallback (~/.cargo/bin/<binary>).
    // CARGO_HOME is explicit; otherwise derive from HOME (macOS/Linux) or USERPROFILE (Windows).
    let cargo_home = std::env::var("CARGO_HOME")
        .ok()
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".cargo"))
        });
    if let Some(cargo_bin) = cargo_home {
        let candidate = cargo_bin.join("bin").join(binary);
        if candidate.exists() {
            return Some(candidate.display().to_string());
        }
    }

    None
}

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
///
/// For claude launches, if `toolbox` is non-empty, also writes `.claude/mcp.json` and passes
/// `--mcp-config <path>` (augment mode — additive, NOT `--strict-mcp-config`). Non-claude CLIs
/// receive no `--mcp-config` flag even when `toolbox` is populated.
pub fn launch_wrapped(
    cli: &WrappedCli,
    unit_description: &str,
    scope: &str,
    phase: &str,
    workdir: &Path,
    timeout: Duration,
    toolbox: &[McpServerSpec],
) -> anyhow::Result<LaunchOutcome> {
    std::fs::create_dir_all(workdir)
        .map_err(|e| anyhow::anyhow!("create sandbox workdir {}: {e}", workdir.display()))?;

    // ── Write the Claude settings.json (the hook config the subprocess reads). ──
    let settings_path = write_claude_settings(workdir, scope, phase)?;

    // ── Write the MCP toolbox config (augment mode) for claude-family CLIs. ──
    // Non-claude CLIs (agy, pi) skip --mcp-config: they have no --mcp-config flag.
    let is_claude = cli.command.contains("claude") || cli.id.contains("claude");
    let mcp_config_path = if is_claude {
        write_mcp_config(workdir, toolbox)?
    } else {
        None
    };

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

    // Augment mode: add --mcp-config ONLY when the toolbox produced a config file.
    if let Some(ref mcp_path) = mcp_config_path {
        command.arg("--mcp-config").arg(mcp_path);
    }

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

    // ── MCP toolbox injection tests ────────────────────────────────────────────

    /// `write_mcp_config` with a non-empty toolbox writes a valid `mcpServers` JSON.
    #[test]
    fn write_mcp_config_produces_valid_json() {
        let dir = std::env::temp_dir().join(format!("wa-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let toolbox = vec![
            McpServerSpec {
                name: "wicked-estate".to_string(),
                command: "/usr/local/bin/wicked-estate-mcp".to_string(),
                args: vec![],
                env: Default::default(),
            },
            McpServerSpec {
                name: "wicked-memory".to_string(),
                command: "/usr/local/bin/wicked-memory-mcp".to_string(),
                args: vec!["--stdio".to_string()],
                env: [("WICKED_MEM_DB".to_string(), "/tmp/mem.db".to_string())]
                    .into_iter()
                    .collect(),
            },
        ];

        let path = write_mcp_config(&dir, &toolbox).unwrap();
        let path = path.expect("non-empty toolbox must produce a path");

        assert!(path.exists(), "mcp.json must exist on disk");

        let content = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).expect("must be valid JSON");

        // Top-level key must be `mcpServers`.
        assert!(
            v.get("mcpServers").is_some(),
            "JSON must have top-level 'mcpServers' key"
        );
        let servers = v["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 2, "two servers must be registered");

        // Estate entry.
        let estate = &servers["wicked-estate"];
        assert_eq!(
            estate["command"].as_str().unwrap(),
            "/usr/local/bin/wicked-estate-mcp"
        );
        // No `args` key when args is empty.
        assert!(estate.get("args").is_none(), "empty args must be omitted");

        // Memory entry: args and env present.
        let memory = &servers["wicked-memory"];
        assert_eq!(
            memory["command"].as_str().unwrap(),
            "/usr/local/bin/wicked-memory-mcp"
        );
        let args = memory["args"].as_array().unwrap();
        assert_eq!(args, &[serde_json::Value::String("--stdio".to_string())]);
        assert_eq!(
            memory["env"]["WICKED_MEM_DB"].as_str().unwrap(),
            "/tmp/mem.db"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `write_mcp_config` with an empty toolbox returns `None` (no file written, no flag added).
    #[test]
    fn write_mcp_config_empty_toolbox_returns_none() {
        let dir = std::env::temp_dir().join(format!("wa-mcp-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = write_mcp_config(&dir, &[]).unwrap();
        assert!(result.is_none(), "empty toolbox must return None");

        // No mcp.json file should have been written.
        assert!(
            !dir.join(".claude").join("mcp.json").exists(),
            "no mcp.json should be written for empty toolbox"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `launch_wrapped` with empty toolbox does NOT add `--mcp-config` to the subprocess command.
    /// Verified by inspecting the subprocess args (fake echo CLI captures its own argv).
    #[cfg(unix)]
    #[test]
    fn launch_wrapped_with_empty_toolbox_omits_mcp_config_flag() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("wa-mcp-no-flag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // A fake "claude" that just prints its argv and exits 0.
        let fake = dir.join("fake-claude");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG:$a\"; done\n",
        )
        .unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();

        unsafe { std::env::set_var("WICKED_AGENT_BIN", &fake) };

        let cli = WrappedCli {
            command: fake.display().to_string(),
            args: vec![],
            mode: GovernanceMode::PretoolHook,
            id: "claude".to_string(),
        };
        let workdir = dir.join("work");
        let outcome = launch_wrapped(
            &cli,
            "do something",
            "scope",
            "exec",
            &workdir,
            std::time::Duration::from_secs(5),
            &[], // empty toolbox
        )
        .unwrap();

        assert!(
            !outcome.stdout.contains("--mcp-config"),
            "empty toolbox must NOT produce --mcp-config in argv; got: {}",
            outcome.stdout
        );

        unsafe { std::env::remove_var("WICKED_AGENT_BIN") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `launch_wrapped` with a non-empty toolbox writes the `mcp.json` to workdir/.claude/,
    /// proving the `--mcp-config` path is generated and would be passed to the subprocess.
    /// We verify the on-disk artifact rather than subprocess argv (avoiding env-var races in
    /// parallel test suites).
    #[cfg(unix)]
    #[test]
    fn launch_wrapped_with_toolbox_includes_mcp_config_flag() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();

        let dir = std::env::temp_dir().join(format!("wa-mcp-with-flag-{pid}-{n}"));
        std::fs::create_dir_all(&dir).unwrap();

        // A minimal fake "claude" that exits 0 immediately.
        let fake = dir.join("fake-claude");
        std::fs::write(&fake, "#!/bin/sh\nexit 0\n").unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();

        // Pin WICKED_AGENT_BIN to the fake binary so write_claude_settings uses it.
        // Use a unique env var name to avoid interfering with parallel tests on the same process.
        // Safety: test-only; test binary is single-threaded per Rust test runner default.
        unsafe { std::env::set_var("WICKED_AGENT_BIN", &fake) };

        // A fake MCP binary — must exist on disk so the path is recorded in mcp.json.
        let fake_mcp = dir.join("fake-mcp-server");
        std::fs::write(&fake_mcp, "#!/bin/sh\n").unwrap();
        let mut p2 = std::fs::metadata(&fake_mcp).unwrap().permissions();
        p2.set_mode(0o755);
        std::fs::set_permissions(&fake_mcp, p2).unwrap();

        let toolbox = vec![McpServerSpec {
            name: "wicked-estate".to_string(),
            command: fake_mcp.display().to_string(),
            args: vec![],
            env: Default::default(),
        }];

        let cli = WrappedCli {
            command: fake.display().to_string(),
            args: vec![],
            mode: GovernanceMode::PretoolHook,
            id: "claude".to_string(),
        };
        let workdir = dir.join("work");
        // launch_wrapped must succeed (exit code is not checked here — governance reads decisions).
        let _outcome = launch_wrapped(
            &cli,
            "do something",
            "scope",
            "exec",
            &workdir,
            std::time::Duration::from_secs(5),
            &toolbox,
        )
        .unwrap();

        // The mcp.json MUST have been written — this is the proof that the --mcp-config path
        // was generated and would be appended to the subprocess args.
        let mcp_json = workdir.join(".claude").join("mcp.json");
        assert!(
            mcp_json.exists(),
            "mcp.json must be written to workdir/.claude/ when toolbox is non-empty"
        );

        // Verify the written mcp.json is valid and contains the expected server entry.
        let content = std::fs::read_to_string(&mcp_json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert!(
            v["mcpServers"]["wicked-estate"].is_object(),
            "mcpServers must contain the wicked-estate entry"
        );
        assert_eq!(
            v["mcpServers"]["wicked-estate"]["command"]
                .as_str()
                .unwrap(),
            fake_mcp.display().to_string(),
            "command must point to the fake MCP binary"
        );

        unsafe { std::env::remove_var("WICKED_AGENT_BIN") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-claude CLI skips `--mcp-config` even when toolbox is non-empty.
    #[cfg(unix)]
    #[test]
    fn launch_wrapped_non_claude_cli_skips_mcp_config() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("wa-mcp-agy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let fake = dir.join("fake-agy");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a in \"$@\"; do echo \"ARG:$a\"; done\n",
        )
        .unwrap();
        let mut p = std::fs::metadata(&fake).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&fake, p).unwrap();

        unsafe { std::env::set_var("WICKED_AGENT_BIN", &fake) };

        let toolbox = vec![McpServerSpec {
            name: "wicked-estate".to_string(),
            command: "/usr/bin/true".to_string(),
            args: vec![],
            env: Default::default(),
        }];

        let cli = WrappedCli {
            command: fake.display().to_string(),
            args: vec![],
            mode: GovernanceMode::PostHoc,
            id: "agy".to_string(), // NOT claude
        };
        let workdir = dir.join("work");
        let outcome = launch_wrapped(
            &cli,
            "do something",
            "scope",
            "exec",
            &workdir,
            std::time::Duration::from_secs(5),
            &toolbox,
        )
        .unwrap();

        assert!(
            !outcome.stdout.contains("--mcp-config"),
            "non-claude CLI must NOT get --mcp-config even with a toolbox; got: {}",
            outcome.stdout
        );

        unsafe { std::env::remove_var("WICKED_AGENT_BIN") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── discover_toolbox / resolve_mcp_binary determinism tests ─────────────────
    //
    // ADVERSARIAL CHALLENGE — env-var determinism. `discover_toolbox` / `resolve_mcp_binary`
    // read PROCESS-GLOBAL env (`WICKED_*_MCP_BIN`, `PATH`, `CARGO_HOME`, `HOME`, `USERPROFILE`).
    // Rust runs tests in PARALLEL, so naive env mutation races. WORSE: this machine may HAVE the
    // three real servers installed in `~/.cargo/bin`, so the cargo-home fallback would resolve a
    // binary regardless of the env-override — a naive "missing → omitted" assertion is NOT
    // deterministic. Each test below LOCKS `ENV_LOCK` (serializing all env-mutating tests against
    // each other) and uses `EnvGuard` to SAVE every env var it touches, SET controlled values, and
    // RESTORE on Drop (so even a panicking assertion cannot leak mutation). To prove "missing →
    // omitted" we must neutralize ALL THREE resolution layers: env-override → nonexistent path,
    // PATH → an empty temp dir, and the cargo-home fallback → CARGO_HOME/HOME/USERPROFILE all
    // pointed at an empty temp dir with no `.cargo/bin/<binary>`.

    /// Serializes every test that mutates process-global env. ALL env-touching tests must lock this.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Save/restore guard for a set of env vars. On construction it records the current value (or
    /// absence) of each named var; on Drop it restores them exactly — present vars are re-set to
    /// their old value, absent vars are removed. This neutralizes leakage across the shared process.
    struct EnvGuard {
        saved: Vec<(String, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        /// Snapshot the current values of `keys`.
        fn capture(keys: &[&str]) -> Self {
            let saved = keys
                .iter()
                .map(|k| ((*k).to_string(), std::env::var_os(k)))
                .collect();
            EnvGuard { saved }
        }

        /// Set a var to a controlled value (within the lock).
        fn set(&self, key: &str, val: impl AsRef<std::ffi::OsStr>) {
            // Safety: serialized by ENV_LOCK; restored on Drop. Matches the file's existing pattern.
            unsafe { std::env::set_var(key, val) };
        }

        /// Remove a var entirely (within the lock).
        fn remove(&self, key: &str) {
            unsafe { std::env::remove_var(key) };
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => unsafe { std::env::set_var(k, val) },
                    None => unsafe { std::env::remove_var(k) },
                }
            }
        }
    }

    /// The full set of env vars `resolve_mcp_binary` reads — every layer's input. We neutralize ALL
    /// of them so each test isolates exactly the layer it intends to exercise.
    const RESOLVER_ENV: &[&str] = &[
        "WICKED_ESTATE_MCP_BIN",
        "WICKED_MEMORY_MCP_BIN",
        "WICKED_KNOWLEDGE_MCP_BIN",
        "PATH",
        "CARGO_HOME",
        "HOME",
        "USERPROFILE",
    ];

    /// Point PATH + CARGO_HOME + HOME + USERPROFILE at an EMPTY temp dir, so the PATH probe and the
    /// cargo-home fallback can resolve NOTHING. Caller still controls the per-server env-overrides.
    /// Returns the empty dir (kept alive for the test's lifetime).
    fn neutralize_path_and_cargo_home(guard: &EnvGuard, tag: &str) -> std::path::PathBuf {
        let empty = std::env::temp_dir().join(format!(
            "wa-empty-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        // Empty PATH dir → PATH probe finds no binaries.
        guard.set("PATH", &empty);
        // CARGO_HOME → <empty>; cargo-home fallback looks at <empty>/bin/<binary> (absent).
        guard.set("CARGO_HOME", &empty);
        // HOME / USERPROFILE → <empty>; the derived `~/.cargo/bin/<binary>` is also absent.
        guard.set("HOME", &empty);
        guard.set("USERPROFILE", &empty);
        empty
    }

    /// env-override → INCLUDED: a `WICKED_ESTATE_MCP_BIN` pointed at a REAL existing file must make
    /// `wicked-estate` appear in the specs with EXACTLY that command path — the override is the
    /// highest-priority layer and wins over PATH and cargo-home.
    #[cfg(unix)]
    #[test]
    fn discover_toolbox_env_override_includes_server_with_exact_path() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        // Neutralize the lower layers so the ONLY thing that can resolve estate is our override.
        let empty = neutralize_path_and_cargo_home(&guard, "ovr");

        // A real existing temp file to stand in for the estate binary.
        let fake_estate = empty.join("real-wicked-estate-mcp");
        std::fs::write(&fake_estate, b"#!/bin/sh\n").unwrap();
        assert!(fake_estate.exists());

        guard.set("WICKED_ESTATE_MCP_BIN", &fake_estate);
        // The other two have NO override and cannot be resolved by any neutralized layer → omitted.
        guard.remove("WICKED_MEMORY_MCP_BIN");
        guard.remove("WICKED_KNOWLEDGE_MCP_BIN");

        let specs = discover_toolbox();

        let estate = specs
            .iter()
            .find(|s| s.name == "wicked-estate")
            .expect("env-override must include wicked-estate");
        assert_eq!(
            estate.command,
            fake_estate.display().to_string(),
            "the override path must be used verbatim as the command"
        );
        assert!(
            !specs.iter().any(|s| s.name == "wicked-memory"),
            "wicked-memory has no override and no resolvable layer → must be omitted"
        );
        assert!(
            !specs.iter().any(|s| s.name == "wicked-knowledge"),
            "wicked-knowledge has no override and no resolvable layer → must be omitted"
        );

        let _ = std::fs::remove_dir_all(&empty);
        // guard drops here → all RESOLVER_ENV restored.
    }

    /// all-missing → OMITTED + empty/graceful: with every override pointed at a NONEXISTENT path AND
    /// PATH + cargo-home neutralized, NO server resolves → `discover_toolbox()` returns an EMPTY Vec,
    /// and `write_mcp_config(workdir, &specs)` returns `None` (no file). This is the graceful-
    /// degradation invariant: a fully-unresolvable toolbox is not a hard error.
    #[cfg(unix)]
    #[test]
    fn discover_toolbox_all_missing_returns_empty_and_no_config() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        let empty = neutralize_path_and_cargo_home(&guard, "missing");

        // Every override points at a path that does NOT exist → layer 1 fails its `.exists()` check.
        let nonexistent = empty.join("does-not-exist-anywhere");
        assert!(!nonexistent.exists());
        guard.set("WICKED_ESTATE_MCP_BIN", &nonexistent);
        guard.set("WICKED_MEMORY_MCP_BIN", &nonexistent);
        guard.set("WICKED_KNOWLEDGE_MCP_BIN", &nonexistent);

        let specs = discover_toolbox();
        assert!(
            specs.is_empty(),
            "no layer can resolve any server → specs must be empty, got: {specs:?}"
        );

        // Graceful degradation: an empty toolbox yields NO mcp.json (and no flag downstream).
        let workdir = empty.join("work");
        let cfg = write_mcp_config(&workdir, &specs).unwrap();
        assert!(cfg.is_none(), "empty specs must produce no mcp.json");
        assert!(
            !workdir.join(".claude").join("mcp.json").exists(),
            "no mcp.json file may be written for an empty toolbox"
        );

        let _ = std::fs::remove_dir_all(&empty);
    }

    /// partial toolbox → GRACEFUL: exactly ONE server resolvable (estate via a real-file override),
    /// the other TWO unresolvable. `discover_toolbox()` must return EXACTLY the one resolvable spec —
    /// proving the harness degrades to a partial toolbox rather than all-or-nothing. The written
    /// mcp.json must contain only that one server.
    #[cfg(unix)]
    #[test]
    fn discover_toolbox_partial_toolbox_includes_only_resolvable() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        let empty = neutralize_path_and_cargo_home(&guard, "partial");

        // estate resolves via a real-file override; memory + knowledge point at a nonexistent path.
        let fake_estate = empty.join("real-wicked-estate-mcp");
        std::fs::write(&fake_estate, b"#!/bin/sh\n").unwrap();
        let nonexistent = empty.join("nope");
        assert!(!nonexistent.exists());
        guard.set("WICKED_ESTATE_MCP_BIN", &fake_estate);
        guard.set("WICKED_MEMORY_MCP_BIN", &nonexistent);
        guard.set("WICKED_KNOWLEDGE_MCP_BIN", &nonexistent);

        let specs = discover_toolbox();
        assert_eq!(
            specs.len(),
            1,
            "exactly one server is resolvable → exactly one spec, got: {specs:?}"
        );
        assert_eq!(specs[0].name, "wicked-estate");
        assert_eq!(specs[0].command, fake_estate.display().to_string());

        // The written config must carry only the resolvable server.
        let workdir = empty.join("work");
        let cfg = write_mcp_config(&workdir, &specs)
            .unwrap()
            .expect("a one-spec toolbox must write a config");
        let content = std::fs::read_to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        let servers = v["mcpServers"].as_object().unwrap();
        assert_eq!(servers.len(), 1, "config must contain exactly one server");
        assert!(servers.contains_key("wicked-estate"));
        assert!(!servers.contains_key("wicked-memory"));
        assert!(!servers.contains_key("wicked-knowledge"));

        let _ = std::fs::remove_dir_all(&empty);
    }

    /// resolve_mcp_binary: an env-override pointing at a NONEXISTENT path must NOT be returned — the
    /// `.exists()` guard rejects it, forcing fall-through to the next layer. With PATH + cargo-home
    /// neutralized, the result is `None`. Pins the exact layer-1 semantics (override is path-checked,
    /// not blindly trusted).
    #[cfg(unix)]
    #[test]
    fn resolve_mcp_binary_nonexistent_override_falls_through_to_none() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        let empty = neutralize_path_and_cargo_home(&guard, "fallthrough");
        let nonexistent = empty.join("ghost-binary");
        guard.set("WICKED_ESTATE_MCP_BIN", &nonexistent);

        let resolved = resolve_mcp_binary("wicked-estate-mcp", "WICKED_ESTATE_MCP_BIN");
        assert!(
            resolved.is_none(),
            "a nonexistent override + neutralized PATH/cargo-home must resolve to None, got: {resolved:?}"
        );

        let _ = std::fs::remove_dir_all(&empty);
    }

    /// resolve_mcp_binary: PATH-probe layer. With the env-override absent and cargo-home neutralized,
    /// a binary placed in a PATH dir must be found and returned with its full path. Proves layer 2.
    #[cfg(unix)]
    #[test]
    fn resolve_mcp_binary_finds_binary_on_path() {
        use std::os::unix::fs::PermissionsExt;
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        let empty = neutralize_path_and_cargo_home(&guard, "pathprobe");
        // Override absent → layer 1 skipped; cargo-home neutralized → layer 3 finds nothing.
        guard.remove("WICKED_MEMORY_MCP_BIN");

        // Put a real executable named exactly like the binary into a PATH dir.
        let path_dir = empty.join("pathbin");
        std::fs::create_dir_all(&path_dir).unwrap();
        let on_path = path_dir.join("wicked-memory-mcp");
        std::fs::write(&on_path, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&on_path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&on_path, perm).unwrap();
        guard.set("PATH", &path_dir);

        let resolved = resolve_mcp_binary("wicked-memory-mcp", "WICKED_MEMORY_MCP_BIN");
        assert_eq!(
            resolved,
            Some(on_path.display().to_string()),
            "the PATH probe must return the binary found on PATH"
        );

        let _ = std::fs::remove_dir_all(&empty);
    }

    /// resolve_mcp_binary: cargo-home fallback (layer 3). With the override absent and PATH empty,
    /// a binary at `$CARGO_HOME/bin/<binary>` must be found. Proves the last-resort layer fires.
    #[cfg(unix)]
    #[test]
    fn resolve_mcp_binary_finds_binary_in_cargo_home() {
        let _lock = ENV_LOCK.lock().unwrap();
        let guard = EnvGuard::capture(RESOLVER_ENV);

        let empty = neutralize_path_and_cargo_home(&guard, "cargohome");
        guard.remove("WICKED_KNOWLEDGE_MCP_BIN");

        // Build a CARGO_HOME with bin/<binary> present.
        let cargo_home = empty.join("cargo");
        let cargo_bin = cargo_home.join("bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let installed = cargo_bin.join("wicked-knowledge-mcp");
        std::fs::write(&installed, b"#!/bin/sh\n").unwrap();
        guard.set("CARGO_HOME", &cargo_home);

        let resolved = resolve_mcp_binary("wicked-knowledge-mcp", "WICKED_KNOWLEDGE_MCP_BIN");
        assert_eq!(
            resolved,
            Some(installed.display().to_string()),
            "the cargo-home fallback must return $CARGO_HOME/bin/<binary>"
        );

        let _ = std::fs::remove_dir_all(&empty);
    }
}
