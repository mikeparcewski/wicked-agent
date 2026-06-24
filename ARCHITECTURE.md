# wicked-agent — Architecture

A Rust crate (`wicked_agent` lib + `wicked-agent` bin) that drives the three siblings — **governance + orchestration + council** — IN-PROCESS on ONE shared wicked-estate store, turning many agent CLIs into a single governed entity through **plan → distribute → execute** under the estate's rigor.

## What it owns vs reuses

| Concern | Owner | Where |
|--------|-------|-------|
| **Session loop** (`run_session` / `run_session_wrapped`: plan → distribute → execute → session result) | **own** | `src/lib.rs` |
| **Plan** (deterministic decomposition of a problem into ordered `WorkUnit`s) | **own** | `src/plan.rs` |
| **Distribute** (convene the council in-process, read the verdict, assign a CLI per unit) | **own** | `src/distribute.rs` |
| **Execute** (open a phase, run the gate, record the outcome on the shared store) | **own** | `src/execute.rs` |
| **Inject** (launch a real wrapped-CLI subprocess + the governance gate + the `gate-hook`) | **own** | `src/inject.rs` |
| **Scope** (shared-vs-isolated entity → collection-scope toggle) | **own** | `src/scope.rs` |
| **CLI** (`run` / `run-real` / `status` / `gate-hook` / `health`) | **own** | `src/main.rs` |
| Council verdict / "which CLI owns this unit" | **reuse** | `wicked-council` (in-process crate dep) |
| Workflow phases, the reducer, the gate | **reuse** | `wicked-orchestration` (in-process crate dep) |
| Policy decision on the work / each tool-call (`select` + `decide` + `conform`) | **reuse** | `wicked-governance` (in-process crate dep) |
| The shared estate store + node/symbol model + the bus emit seam | **reuse** | `wicked-apps-core` over `wicked-estate` (`SqliteStore`, `Node`, `ToNode`/`FromNode`) |

wicked-agent invents no event log, no policy engine, and no CLI registry. It owns the **glue** that composes the three siblings against one estate store; the authoritative records (phases, claims, council task/verdict) are the siblings'. The deps are real in-process crates (`Cargo.toml`): `wicked-governance`, `wicked-orchestration`, `wicked-council`, `wicked-apps-core`, `wicked-estate-core` — no Node harness, no JSON-over-the-wire clients.

## Data model on the estate store

Every entity is a `Node` on the ONE shared `SqliteStore`, round-tripped losslessly through `Node.metadata` via the `ToNode`/`FromNode` impls in `src/lib.rs`:

- **`AgentSession`** → `Node(NodeKind::Other("agent_session"))` (the `AGENT_SESSION` const from `wicked-apps-core`). Holds `workflow_id`, `problem`, `entity_mode`, `collection_scope`, the convened `clis`, and a `SessionStatus` (`Planning → Distributing → Executing → Completed`).
- **`WorkUnit`** → `Node(NodeKind::Other("work_unit"))` (the `WORK_UNIT` const). Created `Pending` by the plan; distribute records `assigned_cli` + `council_task_ref`; execute records `phase_ref`, `conformance_ref`, `phase_status`, `collection_scope`, and the final `UnitStatus` (`Pending → Distributed → Done | Rejected`).
- **work outputs** → `Node(NodeKind::Other("work_output"))` (the `WORK_OUTPUT` const in `execute.rs`), written **only** when the gate approves, tagged with the unit's collection scope.

The orchestration phase nodes, the governance `ConformanceClaim` nodes (via `conform`), and — on the on-disk path — the council's own `council_task`/`council_verdict`/`cli_ranking` nodes ALL land on the SAME store. Symbols use the `wicked-apps` scheme (`SYMBOL_SCHEME`); ids are stable (`<session>:u<ord>` for units) so a re-run is idempotent. `get_session`, `get_work_unit`, and `session_units` read entities back by symbol.

## Modules

- **`plan.rs`** — `plan_units(problem, session_id)`: a hand-rolled, regex-free scanner splits the problem on newlines / sentence terminators (followed by whitespace, so `3.5` stays whole) / semicolons, trims, drops blanks, falls back to one unit. DETERMINISTIC — same input, same ordered units, no model.
- **`distribute.rs`** — `distribute_units` / `distribute_units_on`: for each unit, convene `wicked_council` IN-PROCESS over the session roster (`Worker::queue_blocking` then `poll`), read the verdict's `winning_recommendation`, and assign the matching roster seat. `pick_assignment` gracefully degrades to the first seat on a no-consensus / split / failed council — distribution ALWAYS yields an assignment, never fails a unit.
- **`execute.rs`** + **`inject.rs`** — see the next section. `execute.rs` brackets each unit in an orchestration phase and consumes the governance gate; `inject.rs` launches the real wrapped CLI and runs the per-tool-call gate across the process boundary.
- **`scope.rs`** — `EntityMode::{Shared, Isolated}` and `resolve_scope`: shared pins every unit's output to ONE `wicked-agent/<session>/shared` scope (N hands, one entity); isolated gives each unit its own `wicked-agent/<session>/unit/<id>` scope. Both live on the SAME store; the toggle is purely the scope id the output node carries.
- **`main.rs`** — the `wicked-agent` binary: `run` (stub execute), `run-real` (real subprocess execute), `status` (read a session + units back), `gate-hook` (the generated hook re-invokes this; returns the gate exit code), `health`.

## The execute path + governance gate

A unit's work is bracketed by orchestration and gated by governance, on the one shared store. `execute_unit` advances a freshly-opened `Phase` `Pending → InProgress → ReadyForGate → GateRunning` through the reducer (each a real `apply_event`), then `wicked_governance::select` + `decide` mint a `ConformanceClaim`, and `wicked_orchestration::apply_gate` consumes it: `Deny ⇒ Rejected`, `Allow ⇒ Approved`, `AllowWithConditions ⇒ ApprovedWithConditions`. On approval a `work_output` node is recorded and `conform` persists the claim as durable evidence; on `Deny` NO output is written (the claim is still recorded as evidence of *why*).

The real-CLI path (`execute_unit_wrapped` → `inject::launch_wrapped`) launches the assigned CLI as a REAL `std::process::Command` subprocess in a sandbox workdir, and the gate fires **two ways**:

1. **Unit-level, pre-launch** — `select` + `decide` over the unit's context BEFORE the subprocess starts. A `Deny` means the CLI is NEVER launched: no subprocess, phase `Rejected`, no output.
2. **Per-tool-call hook** (`gate-hook`) — wicked-agent writes a tiny POSIX/`cmd` `PreToolUse` hook into the sandbox that re-invokes this binary's `gate-hook` subcommand (`write_pretool_hook`, resolved via `WICKED_AGENT_BIN`/`current_exe`). The wrapped CLI pipes each PROPOSED tool-call as JSON on stdin; the hook runs the SAME `select` + `decide` against the SAME on-disk store and exits `2` on `Deny`, so the CLI ABORTS the action before the effect lands. The hook **fails closed** — if it can't open the store or decide, it denies. A post-hoc mode (`GovernanceMode::PostHoc`) runs the same engine AFTER the fact for CLIs that can't take a hook, rolling the sandbox back on a deny (`rollback_workdir`, preserving only harness-owned files).

THE INVARIANT (ADR-0003): the gate fires on EVERY unit; a `Deny` STRUCTURALLY blocks approval (orchestration's persisted veto) and a denied effect never stands. This is **mutation-proved** in `tests/full_e2e.rs`: with the gate live, a forbidden write is blocked and the file is ABSENT on disk; neuter the gate and the forbidden file appears — the gate is load-bearing, not decorative.

## Shared store & one-collection note

The harness holds exactly ONE `SqliteStore`. `SqliteStore` owns a non-cloneable `rusqlite::Connection`, so an **in-memory** store is private to its single handle — cross-handle sharing is impossible in memory (`tests/harness_e2e.rs` runs the whole flow on one `SqliteStore::in_memory()` handle). On-disk sharing across all four concerns (agent + governance + orchestration + council) is therefore achieved by **multiple connections to the same WAL file**: `run-real` exports the resolved DB path as `WICKED_ESTATE_DB` so the `gate-hook` child process and the in-process council (`distribute_units_on` opens its own `SqliteStore::open(path)`) all reach the same `brain.db`. `tests/full_e2e.rs` proves it — it writes policies on one handle, drops it, runs the harness on a second handle, and reads everything back via a FRESH `SqliteStore::open(path)` connection. Distribution is sequential and each council `queue_blocking` joins its worker before returning, so writers never collide (WAL, one writer at a time).

## Real-CLI run

`scripts/demo-real-clis.sh` is the R6 demonstration: it builds the binary, writes a problem JSON, and runs `run-real` with the REAL `claude` CLI (headless, via `scripts/claude-wrapped-agent.sh`) as the executing seat, and a REAL council verdict over `claude`/`agy`/`pi`. The wrapper asks `claude -p` to do the real work, then surfaces a `write_file` tool-call and honors `$WICKED_PRETOOL_HOOK` (only writing if the hook allows). Everything persists on one on-disk `brain.db`; the script reads it back via `wicked-agent status` (a fresh connection) and counts `agent_session`/`work_unit`/`phase`/`conformance_claim`/`council_task`/`council_verdict`/`cli_ranking` nodes on the SAME file. The run is honestly NONDETERMINISTIC (real CLIs/models); `agy` errors when not headless in all envs, so it abstains as a voter and the council degrades — the executing `claude` seat is listed first so a degraded verdict still assigns it.

## Build

```sh
cargo test                                  # lib unit tests + harness_e2e + full_e2e
cargo clippy --all-targets -- -D warnings
bash scripts/demo-real-clis.sh              # the real-CLI run (claude + council over claude/agy/pi)
```

Decisions are recorded in [`docs/adr/`](docs/adr/); the accurate quick-start is in [`README.md`](README.md).
