//! wicked-agent CLI — drive the in-process harness on a REAL on-disk shared store.
//!
//! Subcommands:
//!   - `run --file <problem.json>`  : decompose + distribute + execute a session on the shared
//!     store (resolved via `WICKED_ESTATE_DB` or `--db <path>`), then print the session result.
//!     The stub execute path (deterministic, no subprocess).
//!   - `run-real --file <problem.json>` : like `run`, but LAUNCHES the council-assigned CLI as a
//!     REAL subprocess per unit (the R6 full-functional path), GOVERNED + GATED + EVIDENCED.
//!   - `status --session <id>`      : read the session + its units + outcomes back from the store.
//!   - `gate-hook --scope <s> --phase <p> [--db <path>]` : the generated PreToolUse hook re-invokes
//!     THIS subcommand; it reads a proposed tool-call JSON on stdin and exits 2 on a governance Deny
//!     (the wrapped CLI must abort the action). Not for humans.
//!   - `health`                     : crate identity smoke.
//!
//! The shared store is the estate `SqliteStore` (the collection). `run` and `status` open the SAME
//! DB so a session written by `run` is readable by `status` — the persistence the harness guarantees.

use std::process::ExitCode;
use std::time::Duration;

use wicked_agent::{
    get_session, run_session, run_session_wrapped, scope::EntityMode, session_units,
    GovernanceMode, SessionResult,
};
use wicked_apps_core::open_store;
use wicked_council::AgenticCli;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("health");

    // The gate-hook subcommand returns the GATE EXIT CODE (2 = deny), not a generic success/failure.
    if cmd == "gate-hook" {
        let a = &args[1..];
        let scope = flag(a, "--scope").unwrap_or("wicked-agent");
        let phase = flag(a, "--phase").unwrap_or("unit");
        let db = flag(a, "--db");
        let code = wicked_agent::run_gate_hook(scope, phase, db);
        return ExitCode::from(code as u8);
    }

    let result = match cmd {
        "run" => cmd_run(&args[1..]),
        "run-real" => cmd_run_real(&args[1..]),
        "status" => cmd_status(&args[1..]),
        "health" => {
            println!("{}", wicked_agent::health());
            Ok(())
        }
        other => Err(anyhow::anyhow!(
            "unknown command {other:?}; expected one of: run, run-real, status, gate-hook, health"
        )),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wicked-agent: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parse a `--flag value` pair out of `args`, returning the value if present.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `run --file <problem.json> [--db <path>] [--entity-mode shared|isolated]`.
///
/// The problem JSON shape:
/// ```json
/// {
///   "problem": "First task.\nSecond task",
///   "entity_mode": "shared",
///   "session_id": "optional-stable-id",
///   "clis": [
///     { "key": "claude", "binary": "claude", "headless_invocation": "claude -p \"{PROMPT}\"" }
///   ]
/// }
/// ```
/// `clis` may be a list of full `AgenticCli` records (serde) OR omitted (then the council registry
/// built-ins are used).
fn cmd_run(args: &[String]) -> anyhow::Result<()> {
    let file = flag(args, "--file")
        .ok_or_else(|| anyhow::anyhow!("run requires --file <problem.json>"))?;
    let raw = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("read problem file {file:?}: {e}"))?;
    let spec: ProblemSpec = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse problem file {file:?}: {e}"))?;

    let entity_mode = flag(args, "--entity-mode")
        .map(EntityMode::parse)
        .unwrap_or_else(|| spec.entity_mode());

    let clis = spec.resolve_clis()?;
    if clis.is_empty() {
        anyhow::bail!("no council CLI seats resolved (provide `clis` in the JSON or a registry)");
    }

    // The ONE shared store: resolved from --db, else WICKED_ESTATE_DB, else the estate default path.
    let db = flag(args, "--db");
    let mut store = open_store(db)?;

    let result = run_session(
        &mut store,
        clis,
        &spec.problem,
        entity_mode,
        spec.session_id.as_deref(),
    )?;

    print_result(&result);
    Ok(())
}

/// `run-real --file <problem.json> [--db <path>] [--entity-mode shared|isolated]
///           [--governance-mode pretool-hook|post-hoc] [--sandbox <dir>] [--timeout-secs <n>]`.
///
/// Drives the REAL wrapped-CLI path: the council-assigned CLI is launched as a subprocess per unit.
/// The on-disk DB path is exported as `WICKED_ESTATE_DB` so the generated governance hook (a child
/// process) opens the SAME shared store the in-process engine wrote the policies to.
fn cmd_run_real(args: &[String]) -> anyhow::Result<()> {
    let file = flag(args, "--file")
        .ok_or_else(|| anyhow::anyhow!("run-real requires --file <problem.json>"))?;
    let raw = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("read problem file {file:?}: {e}"))?;
    let spec: ProblemSpec = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("parse problem file {file:?}: {e}"))?;

    let entity_mode = flag(args, "--entity-mode")
        .map(EntityMode::parse)
        .unwrap_or_else(|| spec.entity_mode());
    let governance_mode = match flag(args, "--governance-mode") {
        Some("post-hoc") => GovernanceMode::PostHoc,
        _ => GovernanceMode::PretoolHook,
    };
    let timeout = flag(args, "--timeout-secs")
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(wicked_agent::DEFAULT_CLI_TIMEOUT);
    let sandbox = flag(args, "--sandbox")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("wicked-agent-sandbox"));

    let clis = spec.resolve_clis()?;
    if clis.is_empty() {
        anyhow::bail!("no council CLI seats resolved (provide `clis` in the JSON or a registry)");
    }

    // The ONE shared on-disk store: resolved from --db, else WICKED_ESTATE_DB, else the default path.
    let db = flag(args, "--db");
    // Export the resolved DB path so the gate-hook child process opens the SAME store.
    if let Some(p) = db {
        unsafe { std::env::set_var("WICKED_ESTATE_DB", p) };
    }
    let mut store = open_store(db)?;

    let result = run_session_wrapped(
        &mut store,
        clis,
        &spec.problem,
        entity_mode,
        spec.session_id.as_deref(),
        governance_mode,
        &sandbox,
        timeout,
    )?;

    print_result(&result);
    Ok(())
}

/// `status --session <id> [--db <path>]` — read the session + units + outcomes back from the store.
fn cmd_status(args: &[String]) -> anyhow::Result<()> {
    let session_id =
        flag(args, "--session").ok_or_else(|| anyhow::anyhow!("status requires --session <id>"))?;
    let db = flag(args, "--db");
    let store = open_store(db)?;

    let Some(session) = get_session(&store, session_id)? else {
        anyhow::bail!("no session {session_id:?} found on the store");
    };
    let units = session_units(&store, session_id)?;

    let report = serde_json::json!({
        "session": session,
        "units": units,
        "unit_count": units.len(),
        "approved": units.iter().filter(|u| matches!(u.status, wicked_agent::UnitStatus::Done)).count(),
        "rejected": units.iter().filter(|u| matches!(u.status, wicked_agent::UnitStatus::Rejected)).count(),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn print_result(result: &SessionResult) {
    match serde_json::to_string_pretty(result) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("wicked-agent: could not serialize result: {e}"),
    }
}

/// The `run --file` problem JSON.
#[derive(Debug, serde::Deserialize)]
struct ProblemSpec {
    problem: String,
    #[serde(default)]
    entity_mode: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    /// Full council CLI records (serde of `AgenticCli`). When omitted, the registry built-ins load.
    #[serde(default)]
    clis: Vec<AgenticCli>,
    /// Optional path to a council registry TOML (merged over the built-ins) when `clis` is empty.
    #[serde(default)]
    registry_toml: Option<String>,
}

impl ProblemSpec {
    fn entity_mode(&self) -> EntityMode {
        self.entity_mode
            .as_deref()
            .map(EntityMode::parse)
            .unwrap_or(EntityMode::Shared)
    }

    /// Resolve the convened roster: explicit `clis` if given, else the council registry built-ins
    /// (optionally merged with `registry_toml`), keeping only seats enabled for council.
    fn resolve_clis(&self) -> anyhow::Result<Vec<AgenticCli>> {
        if !self.clis.is_empty() {
            return Ok(self.clis.clone());
        }
        let toml = self.registry_toml.as_deref().map(std::path::Path::new);
        let clis = wicked_council::registry::load(toml)
            .map_err(|e| anyhow::anyhow!("load council registry: {e}"))?
            .into_iter()
            .filter(|c| c.enabled_for_council)
            .collect();
        Ok(clis)
    }
}
