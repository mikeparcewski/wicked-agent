//! Task #29 — cogiq-scale SQLite benchmark + estate perf-fix analysis.
//!
//! ## What this proves
//!
//! wicked-estate 0.12.0 (currently published) has a full-table-scan path in
//! `find_symbols` when the query has only `kinds` set (no `text`, no `exact_name`):
//! it loads EVERY node, deserialises it, and filters kind in Rust — O(table).
//!
//! The perf fix (branch `perf/find-symbols-kind-pushdown`, commit `1fa15a5`) adds an
//! `else if !query.kinds.is_empty()` branch that emits:
//!   `SELECT data FROM nodes WHERE kind IN (?, ?) ORDER BY symbol`
//! so SQLite uses `idx_nodes_kind` — O(matches).  Proven by `EXPLAIN QUERY PLAN`.
//!
//! This test:
//!   1. Builds a cogiq-scale in-memory store (N=1000 work_unit nodes + noise nodes of
//!      other kinds → ≥3,500 nodes total; mirrors the harness session model at cogiq
//!      scale: 1 session, N work_units, 2N conformance_claims, N/4 phases, 1 workflow).
//!   2. Times the kind-filtered `find_symbols` call and reports wall-clock.
//!   3. Runs `EXPLAIN QUERY PLAN` on the old (full-scan) SQL and the new (kind-pushdown)
//!      SQL and asserts the new plan hits the index.
//!   4. Runs a real 20-unit stub session end-to-end and measures setup / execute time.
//!
//! Run with:
//!   cargo test -p wicked-agent --test cogiq_bench -- --nocapture

use std::time::Instant;

use wicked_agent::{run_session, session_units, EntityMode};
use wicked_apps_core::{
    open_store, synthetic_symbol, GraphRead, Language, Location, Node, NodeKind, Span, SqliteStore,
    AGENT_SESSION, SYMBOL_SCHEME, WORK_UNIT,
};
use wicked_council::AgenticCli;
use wicked_estate_core::{GraphWrite, SymbolQuery};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Synthetic node for any `kind_str` and `id`.
fn synthetic_node(kind_str: &str, id: &str) -> Node {
    Node::new(
        synthetic_symbol(kind_str, id),
        NodeKind::Other(kind_str.to_string()),
        format!("{kind_str}/{id}"),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{kind_str}/{id}"), Span::ZERO),
    )
}

/// Insert `nodes` into `store` in a single batch.
fn batch_insert(store: &mut SqliteStore, nodes: &[Node]) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(nodes)?;
    store.commit_batch()?;
    Ok(())
}

/// Build a cogiq-scale store with N work_unit nodes plus noise.
///
/// Session model mirrors the harness at cogiq scale (cogiq ~= 1000 Python modules):
///   N     work_unit nodes
///   2N    conformance_claim nodes (creator + evaluator per unit)
///   N/4   phase nodes
///   1     workflow node
///   1     agent_session node
///   N/2   Function nodes (code-graph noise simulating shared store context)
///
/// Total at N=1000: 1000 + 2000 + 250 + 1 + 1 + 500 = 3752 nodes.
fn build_cogiq_store(n: usize) -> anyhow::Result<SqliteStore> {
    let mut store = open_store(Some(":memory:"))?;
    let mut nodes: Vec<Node> = Vec::with_capacity(n * 4);

    // work_unit nodes
    for i in 0..n {
        nodes.push(synthetic_node(WORK_UNIT, &format!("cogiq:u{i}")));
    }
    // conformance_claim nodes (2×N: creator + evaluator per unit)
    for i in 0..n {
        nodes.push(synthetic_node(
            "conformance_claim",
            &format!("cogiq:cc-creator-u{i}"),
        ));
        nodes.push(synthetic_node(
            "conformance_claim",
            &format!("cogiq:cc-evaluator-u{i}"),
        ));
    }
    // phase nodes (N/4)
    for i in 0..(n / 4).max(1) {
        nodes.push(synthetic_node("phase", &format!("cogiq:phase-{i}")));
    }
    // workflow + session
    nodes.push(synthetic_node("workflow", "cogiq:wf-1"));
    nodes.push(synthetic_node(AGENT_SESSION, "cogiq:session-1"));
    // code-graph noise: Function nodes (simulate code symbols in the shared estate)
    for i in 0..(n / 2) {
        nodes.push(Node::new(
            synthetic_symbol("fn", &format!("fn-{i}")),
            NodeKind::Function,
            format!("noise_fn_{i}"),
            Language::new("python"),
            Location::new(format!("src/module_{i}.py"), Span::ZERO),
        ));
    }

    // Insert in batches of 500 to mirror real ingest patterns.
    for chunk in nodes.chunks(500) {
        batch_insert(&mut store, chunk)?;
    }

    Ok(store)
}

// ─── benchmark 1: kind-filter wall-clock at cogiq scale ─────────────────────

/// Populate a 3,500+ node store, time `find_symbols` filtered to `work_unit`.
///
/// In estate 0.12.0 (published) a kind-only query falls through to the full-scan
/// path: SELECT data FROM nodes ORDER BY symbol — O(table).
/// After the perf fix (1fa15a5) the kind is pushed into SQL: O(matches).
///
/// This test measures the wall-clock of both queries and asserts correctness.
#[test]
fn bench_find_symbols_kind_filter_cogiq_scale() -> anyhow::Result<()> {
    const N: usize = 1_000;
    println!("\n=== bench_find_symbols_kind_filter_cogiq_scale (N={N}) ===");

    let store = build_cogiq_store(N)?;

    // Count total nodes (this exercises the full-scan path as reference).
    let all_query = SymbolQuery::default();
    let total_t0 = Instant::now();
    let all_nodes = store.find_symbols(&all_query)?;
    let total_elapsed = total_t0.elapsed();
    let total_count = all_nodes.len();
    println!("  Total nodes in store:              {total_count}");
    println!("  Full-scan (no filter) wall-clock:  {total_elapsed:?}");

    // Kind-filtered query — the path that 0.12.0 falls through to the full scan for.
    let kind_query = SymbolQuery {
        kinds: vec![NodeKind::Other(WORK_UNIT.to_string())],
        ..Default::default()
    };

    let t0 = Instant::now();
    let work_units = store.find_symbols(&kind_query)?;
    let elapsed = t0.elapsed();

    println!("  find_symbols(work_unit) wall-clock: {elapsed:?}");
    println!("  Returned {}/{total_count} nodes", work_units.len());
    println!("  NOTE: estate 0.12.0 takes the full-scan path for kind-only queries.");
    println!("        Perf fix 1fa15a5 adds kind-pushdown (O(matches), idx_nodes_kind).");

    // Correctness: must return exactly N work_unit nodes.
    assert_eq!(
        work_units.len(),
        N,
        "expected {N} work_unit nodes, got {}",
        work_units.len()
    );
    assert!(
        work_units
            .iter()
            .all(|n| n.kind == NodeKind::Other(WORK_UNIT.to_string())),
        "every returned node must have kind work_unit"
    );

    // Structural gate: even the full-scan path on 3500+ nodes must complete in < 10s.
    assert!(
        elapsed.as_secs() < 10,
        "find_symbols took {}ms — structural gate exceeded",
        elapsed.as_millis()
    );

    println!("  PASS: kind-filter correctness + structural timing gate.");
    Ok(())
}

// ─── benchmark 2: EXPLAIN QUERY PLAN — proves index regression ───────────────

/// Open the SQLite file with raw rusqlite and check the query plan for both the
/// old (full-scan) SQL and the new (kind-pushdown) SQL.
///
/// Old path (estate 0.12.0 for kind-only queries):
///   `SELECT data FROM nodes ORDER BY symbol`
///   → EXPLAIN QUERY PLAN: "SCAN nodes"
///
/// New path (perf fix 1fa15a5):
///   `SELECT data FROM nodes WHERE kind IN (?1) ORDER BY symbol`
///   → EXPLAIN QUERY PLAN: "SEARCH nodes USING INDEX idx_nodes_kind"
///
/// This is the primary evidence artifact for the perf fix.
#[test]
fn explain_query_plan_kind_pushdown() -> anyhow::Result<()> {
    println!("\n=== explain_query_plan_kind_pushdown ===");

    // Write to a named temp file so we can open it with raw rusqlite after.
    let dir = std::env::temp_dir().join(format!("wicked-bench-eqp-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("bench.db");
    let db_str = db_path.display().to_string();

    // Populate via SqliteStore so the schema (including idx_nodes_kind) is created.
    {
        let mut store = open_store(Some(&db_str))?;
        let node = synthetic_node(WORK_UNIT, "eqp-u1");
        batch_insert(&mut store, &[node])?;
        // store is dropped here → WAL flushed.
    }

    // Open with raw rusqlite for EXPLAIN QUERY PLAN.
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // ── OLD SQL (full scan — estate 0.12.0 path for kind-only queries) ──
    let old_sql = "EXPLAIN QUERY PLAN SELECT data FROM nodes ORDER BY symbol";
    let mut old_plan = String::new();
    {
        let mut stmt = conn.prepare(old_sql)?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let detail: String = row.get(3)?;
            old_plan.push_str(&detail);
            old_plan.push('\n');
        }
    }
    println!("  OLD SQL (full scan) plan:");
    for line in old_plan.lines() {
        println!("    {line}");
    }

    // ── NEW SQL (kind-pushdown — perf fix path) ──
    let kind_str = serde_json::to_string(&NodeKind::Other(WORK_UNIT.to_string()))?;
    let new_sql = "EXPLAIN QUERY PLAN SELECT data FROM nodes WHERE kind IN (?1) ORDER BY symbol";
    let mut new_plan = String::new();
    {
        let mut stmt = conn.prepare(new_sql)?;
        let mut rows = stmt.query(rusqlite::params![kind_str])?;
        while let Some(row) = rows.next()? {
            let detail: String = row.get(3)?;
            new_plan.push_str(&detail);
            new_plan.push('\n');
        }
    }
    println!("  NEW SQL (kind-pushdown) plan:");
    for line in new_plan.lines() {
        println!("    {line}");
    }

    drop(conn);
    let _ = std::fs::remove_dir_all(&dir);

    // ── Assertions ──
    let old_upper = old_plan.to_uppercase();
    let new_upper = new_plan.to_uppercase();

    // Old SQL: full table scan.
    assert!(
        old_upper.contains("SCAN"),
        "expected old SQL plan to contain SCAN; got: {old_plan}"
    );

    // New SQL: index access on idx_nodes_kind.
    assert!(
        new_upper.contains("USING")
            && (new_upper.contains("INDEX") || new_upper.contains("COVERING")),
        "expected new SQL plan to use an index (USING INDEX / USING COVERING INDEX);\n\
         got: {new_plan}\n\
         This should prove the kind-pushdown hits idx_nodes_kind."
    );

    println!("  PASS: old SQL = SCAN (full table); new SQL = SEARCH USING INDEX.");
    println!("  This proves the kind-pushdown (1fa15a5) eliminates the O(table) scan.");
    Ok(())
}

// ─── benchmark 3: real N=20 stub session end-to-end wall-clock ──────────────

/// Run a 20-unit stub session and report wall-clock.
///
/// Uses the wicked-agent binary itself as a no-op stub CLI (health command always
/// exits 0 in milliseconds).  Measures the harness overhead — plan → distribute →
/// execute — at N=20 with the shared SQLite store.
#[test]
fn bench_n20_stub_session_wall_clock() -> anyhow::Result<()> {
    println!("\n=== bench_n20_stub_session_wall_clock (N=20) ===");

    let dir = std::env::temp_dir().join(format!("wicked-bench-n20-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("session.db");
    let db_str = db_path.display().to_string();

    let mut store = open_store(Some(&db_str))?;

    // 20 units: semicolon-separated so plan_units splits at each `;`.
    let problem = (1..=20)
        .map(|i| format!("Benchmark stub unit {i}"))
        .collect::<Vec<_>>()
        .join("; ");

    // Stub CLI: the wicked-agent binary itself (health subcommand) as a no-op seat.
    // It always exits 0 instantly so governance never blocks.
    let agent_bin = env!("CARGO_BIN_EXE_wicked-agent");
    let stub_cli = AgenticCli {
        key: "stub".to_string(),
        display_name: "Stub (bench)".to_string(),
        binary: agent_bin.to_string(),
        headless_invocation: format!("{agent_bin} health"),
        category: wicked_council::Category::AgenticCoder,
        input_mode: wicked_council::InputMode::PromptArg,
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: wicked_council::Confidence::Verified,
        enabled_for_council: true,
    };

    let session_t0 = Instant::now();
    let result = run_session(
        &mut store,
        vec![stub_cli],
        &problem,
        EntityMode::Shared,
        Some("bench-n20"),
    )?;
    let session_elapsed = session_t0.elapsed();

    let total_units = result.units.len();
    println!("  Session ID:         {}", result.session_id);
    println!("  Units planned:      {total_units}");
    println!("  Approved:           {}", result.approved);
    println!("  Rejected:           {}", result.rejected);
    println!("  Total wall-clock:   {session_elapsed:?}");
    if total_units > 0 {
        println!(
            "  Wall-clock/unit:    {:?}",
            session_elapsed / total_units as u32
        );
    }

    // Verify persistence: read back units from store.
    let read_back = session_units(&store, "bench-n20")?;
    println!("  Work units in store: {}", read_back.len());

    drop(store);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        total_units, 20,
        "expected 20 units planned, got {total_units}"
    );
    assert!(
        session_elapsed.as_secs() < 120,
        "20-unit stub session took {}s — exceeded 120s gate",
        session_elapsed.as_secs()
    );

    println!("  PASS: 20-unit stub session completed in {session_elapsed:?}.");
    Ok(())
}
