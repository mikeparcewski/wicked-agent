//! FUNCTIONAL SERVE PROOF — the real wicked-* MCP servers actually serve their tools.
//!
//! This is the FUNCTIONAL-PROOF half of the MCP toolbox feature: it is not enough that the harness
//! *discovers* and *wires* the servers (proven hermetically in `inject.rs` / `execute.rs`); the
//! discovered binaries must genuinely speak MCP and expose the tool roster the toolbox promises.
//!
//! Each test launches a REAL server binary as a subprocess, performs the genuine newline-delimited
//! JSON-RPC 2.0 stdio handshake (`initialize` then `tools/list`, protocol `2024-11-05` — verified
//! against the server sources), parses the response, and asserts the EXACT tool-name set:
//!   - `wicked-memory-mcp`    serves 6 tools.
//!   - `wicked-knowledge-mcp` serves 7 tools.
//!
//! Hermetic w.r.t. data: each server's DB env is pointed at a fresh temp path (`WICKED_MEMORY_DB`
//! for memory; `WICKED_KNOWLEDGE_DB` + `WICKED_XEDGE_DB` for knowledge) so the proof never touches
//! real stores. Robust: bounded read with a deadline; the child is killed on drop via `ChildGuard`.
//!
//! `#[ignore]` + binary-presence gate: a CI box WITHOUT the binaries skips cleanly (the test returns
//! early with an explanatory eprintln rather than failing). Run explicitly:
//!   `cargo test --test mcp_serve_proof -- --ignored`

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Kills the child process when dropped — guarantees no leaked MCP server even if an assertion
/// panics mid-test.
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Resolve an MCP server binary the same way production does: env-override (`.exists()`-checked) →
/// PATH probe → `$CARGO_HOME/bin` or `~/.cargo/bin` fallback. Returns `None` when not found anywhere.
fn resolve_bin(binary: &str, env_var: &str) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env_var) {
        let path = PathBuf::from(&p);
        if !p.is_empty() && path.exists() {
            return Some(path);
        }
    }
    if let Ok(path_val) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_val) {
            let candidate = dir.join(binary);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    let cargo_home = std::env::var("CARGO_HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .map(|h| PathBuf::from(h).join(".cargo"))
        });
    if let Some(home) = cargo_home {
        let candidate = home.join("bin").join(binary);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// A fresh, unique temp dir for a server's databases (unique per binary + pid + nanos).
fn fresh_db_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("wa-serve-{tag}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp db dir");
    dir
}

/// Drive ONE server: spawn it with the given DB env, send `initialize` then `tools/list`, and return
/// the sorted Vec of tool names from the `tools/list` result. Bounded by a per-line read deadline.
fn serve_tool_names(bin: &PathBuf, db_env: &[(&str, PathBuf)], server_label: &str) -> Vec<String> {
    let mut command = Command::new(bin);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in db_env {
        command.env(k, v);
    }
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {server_label}: {e}"));

    // Write both requests up front (the server replies to each line independently).
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        // `initialize` first (faithful to the MCP handshake), then `tools/list`.
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n")
            .expect("write initialize");
        stdin
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .expect("write tools/list");
        stdin.flush().expect("flush stdin");
    }
    // Drop stdin so the server sees EOF after our two requests and exits cleanly when we're done.
    drop(child.stdin.take());

    let stdout = child.stdout.take().expect("child stdout");
    let guard = ChildGuard(child); // kills on drop from here on
    let mut reader = BufReader::new(stdout);

    // Read lines until we get the response with id==2 (tools/list) or hit the deadline.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut tools: Option<Vec<String>> = None;
    let mut saw_init = false;

    while Instant::now() < deadline {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap_or(0);
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = v.get("id").and_then(serde_json::Value::as_i64);
        if id == Some(1) {
            // initialize response — sanity check the protocol version + serverInfo.
            saw_init = true;
            assert_eq!(
                v["result"]["protocolVersion"].as_str(),
                Some("2024-11-05"),
                "{server_label} initialize must report protocolVersion 2024-11-05"
            );
            assert!(
                v["result"]["serverInfo"]["name"].is_string(),
                "{server_label} initialize must report a serverInfo.name"
            );
        } else if id == Some(2) {
            let arr = v["result"]["tools"]
                .as_array()
                .unwrap_or_else(|| panic!("{server_label} tools/list result.tools must be an array"));
            let mut names: Vec<String> = arr
                .iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect();
            names.sort();
            tools = Some(names);
            break;
        }
    }

    drop(guard); // explicit kill + reap
    assert!(saw_init, "{server_label} never answered initialize before tools/list");
    tools.unwrap_or_else(|| panic!("{server_label} did not return a tools/list response in time"))
}

/// FUNCTIONAL PROOF — `wicked-memory-mcp` serves its 6 tools over real JSON-RPC stdio.
#[ignore]
#[test]
fn memory_mcp_serves_six_tools() {
    let Some(bin) = resolve_bin("wicked-memory-mcp", "WICKED_MEMORY_MCP_BIN") else {
        eprintln!("SKIP: wicked-memory-mcp not found (env/PATH/~/.cargo/bin) — install to run this proof");
        return;
    };

    let db_dir = fresh_db_dir("memory");
    let names = serve_tool_names(
        &bin,
        &[("WICKED_MEMORY_DB", db_dir.join("memory.db"))],
        "wicked-memory-mcp",
    );

    let mut expected = vec![
        "memory.capture",
        "memory.coverage",
        "memory.erase",
        "memory.learn",
        "memory.recall",
        "memory.reflect",
    ];
    expected.sort();

    assert_eq!(
        names.len(),
        6,
        "wicked-memory-mcp must serve exactly 6 tools; got {names:?}"
    );
    assert_eq!(
        names,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "wicked-memory-mcp tool roster must match the documented 6 tools exactly"
    );

    let _ = std::fs::remove_dir_all(&db_dir);
}

/// FUNCTIONAL PROOF — `wicked-knowledge-mcp` serves its 7 tools over real JSON-RPC stdio.
#[ignore]
#[test]
fn knowledge_mcp_serves_seven_tools() {
    let Some(bin) = resolve_bin("wicked-knowledge-mcp", "WICKED_KNOWLEDGE_MCP_BIN") else {
        eprintln!(
            "SKIP: wicked-knowledge-mcp not found (env/PATH/~/.cargo/bin) — install to run this proof"
        );
        return;
    };

    let db_dir = fresh_db_dir("knowledge");
    let names = serve_tool_names(
        &bin,
        &[
            ("WICKED_KNOWLEDGE_DB", db_dir.join("knowledge.db")),
            ("WICKED_XEDGE_DB", db_dir.join("xedge.db")),
        ],
        "wicked-knowledge-mcp",
    );

    let mut expected = vec![
        "knowledge.coverage",
        "knowledge.ingest",
        "knowledge.recall",
        "knowledge.recall_about_code",
        "knowledge.relate",
        "knowledge.relate_code",
        "knowledge.write",
    ];
    expected.sort();

    assert_eq!(
        names.len(),
        7,
        "wicked-knowledge-mcp must serve exactly 7 tools; got {names:?}"
    );
    assert_eq!(
        names,
        expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "wicked-knowledge-mcp tool roster must match the documented 7 tools exactly"
    );

    let _ = std::fs::remove_dir_all(&db_dir);
}
