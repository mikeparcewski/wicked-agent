# wicked-agent — Architecture

**Status:** design. Shared contracts in [`../wicked-governance/docs/REUSE-MAP.md`](../wicked-governance/docs/REUSE-MAP.md) (the spine — it wins on any conflict); decisions in [`docs/adr/`](docs/adr/).

wicked-agent is a Node agent-tool (mirrors `wicked-brain` / `wicked-testing`). It is the **capstone** of the four-app set: the harness/execution layer that wraps one or more agent CLIs and drives them through **plan → distribute → execute** under the estate's rigor. It owns only the **session loop, the per-CLI injection, and the single-entity-vs-separate toggle**. It reuses everything else: `wicked-council` for distribution, `wicked-orchestration` for the event/phase backbone, `wicked-governance` for the rigor gate, the **collection** for shared state, `wicked-vault` for evidence, `wicked-bus` to coordinate. The discipline: *don't invent a primitive — reuse a sibling; if you can't, the design is wrong.*

## 1. What it owns vs. reuses

| Concern | Owner | Where |
|--------|-------|-------|
| **Session loop** (the interactive flow: repos→CLIs→problem→plan→distribute→execute) | **own** | `lib/session.mjs` |
| **Per-CLI injection** (install skills dir, wire governance hook, set env, capture outputs) | **own** | `lib/inject.mjs` |
| **Single-entity-vs-separate toggle** (shared vs isolated collection scope) | **own** | `lib/scope.mjs` |
| **Session ledger** (sessions, wrapped-clis, work-units — local projection) | **own** | local SQLite |
| Agent-CLI discovery / probe / capability matrix | **reuse** | `wicked-council` (registry + probe) |
| Work distribution / "best CLI for this unit of work" (memory-ranked) | **reuse** | `wicked-council` (verdict + `wicked.cli.ranked` memory) |
| Workflow state, phases, gates, the event backbone | **reuse** | `wicked-orchestration` |
| Rigor/policy decision on each wrapped CLI's tool-calls | **reuse** | `wicked-governance` (`PreToolUse` hook) |
| Shared memory / knowledge / code-graph across CLIs | **reuse** | collection (`wicked-memory` / `wicked-knowledge` / `wicked-overlay`) |
| Tamper-evident evidence of work artifacts | **reuse** | `wicked-vault` |
| Inter-app coordination events | **reuse** | `wicked-bus` |

wicked-agent stores **no CLI registry, no event log, no policy store, and no evidence of its own** — those are council, orchestration, governance, and vault respectively. It keeps only a derived **session ledger** (which CLIs were wrapped, which units of work ran, which scope, and the pointers back into the siblings). The ledger is a projection: if it's lost, the authoritative record still lives in orchestration (events), council (rankings), governance (conformance), and vault (evidence). This is the "don't build a third storage layer" rule from the reuse map, applied to a fourth.

## 2. wicked-agent sits ATOP the other three

```
                          ┌─────────────────────────────────────────────┐
   user ──► run ─────────►│                wicked-agent                  │
                          │  session loop · per-CLI injection · scope    │
                          │  emits wicked.agent.session.*                │
                          └───────┬───────────────┬───────────────┬──────┘
                  distribute      │       phases  │      rigor     │   shared state
                                  ▼               ▼                ▼        │
                        ┌──────────────┐ ┌──────────────────┐ ┌────────────────┐
                        │ wicked-      │ │ wicked-           │ │ wicked-        │
                        │ council      │ │ orchestration     │ │ governance     │
                        │ probe·rank·  │ │ workflow·phase·   │ │ PreToolUse gate│
                        │ pick CLI     │ │ gate (the backbone)│ │ allow/deny/obl │
                        └──────┬───────┘ └────────┬──────────┘ └───────┬────────┘
                               │                  │                    │
                               ▼                  ▼                    ▼
                          wicked-memory       wicked-bus          collection +
                          (CLI rankings)   (fire-and-forget)      wicked-vault
                                                                  (shared scope
                                                                   + evidence)

        wrapped CLIs (claude, gemini, …), launched by wicked-agent into an environment with:
        (a) wicked skills installed   (b) governance PreToolUse hook wired   (c) orchestration
        phase events bracketing the work   (d) outputs captured to the shared collection scope
        (e) evidence recorded to vault     ── this is how many CLIs act as ONE governed entity.
```

The three middle apps are **lane-disjoint building blocks** (REUSE-MAP §1). wicked-agent is the only one that touches all of them; the value is the interlock, not new machinery.

## 3. The session lifecycle

A wicked-agent session **is** a `wicked-orchestration` workflow. The interactive flow maps 1:1 to commands→events→phases; wicked-agent emits the `agent.session.*` vocabulary (REUSE-MAP §3.2) and consumes orchestration's `phase.*` and council's `council.voted`.

```
  repos        CLIs            problem         PLAN            DISTRIBUTE         EXECUTE
   │            │                │              │                  │                 │
   ▼            ▼                ▼              ▼                  ▼                 ▼
 pin scope  council.probe   capture intent  decompose        council per unit    phase per unit
            (capability     ──► .started     into units      ask "best CLI?"     of work + gate
             matrix)                          .plan.created   consume             ── wrapped CLI
                                                              council.voted        runs injected ──
                                                              .work.distributed    .task.completed
                                                                                          │
                                                                                          ▼
                                                                              .session.completed
```

1. **Repos** — the user names the repo(s); they become the session **scope** (a vault scope and a collection scope). One session, one workflow id (`correlationId`).
2. **CLIs** — the user names agent CLIs. wicked-agent **reuses council's registry + usability probe** (REUSE-MAP §4) to learn which CLIs exist, work (not just installed), and how each is invoked — no re-implementation of discovery (ADR-0001). On top of that substrate it runs its own **injection-capability** check — *does this CLI accept a skills dir? a `PreToolUse` hook? env injection?* — which is a harness-specific question council's usability probe does not answer. The two compose into a per-CLI **capability matrix**; the injection dimension is wicked-agent's (and is contributed back as fields on council's shared registry record, not a parallel CLI discovery).
3. **Problem** — free-text intent + the work to be done. `wicked.agent.session.started` fires.
4. **PLAN** — decompose the problem into ordered **units of work**. `wicked.agent.plan.created` fires. (The plan is wicked-agent's; the *ordering/gating* will be an orchestration workflow definition.)
5. **DISTRIBUTE** — for each unit, ask **council** which CLI is best for that work-kind (council ranks by `wicked-memory`). wicked-agent **consumes `wicked.council.voted`** and assigns the unit. `wicked.agent.work.distributed` fires. This is council's decision, not a re-derived one.
6. **EXECUTE** — each unit of work runs as a `wicked-orchestration` **phase with a gate**. wicked-agent launches the assigned CLI **injected** (§5), brackets it with `wicked.phase.started` … `wicked.phase.ready-for-gate`, records evidence to vault, captures outputs to the shared collection scope, and emits `wicked.agent.task.completed`. When all phases pass their gates, `wicked.agent.session.completed` fires.

**Engagement governs the reaction, not the gate** (REUSE-MAP §6.4): an `auto` session lets passing gates advance unattended; an `ask-human` session pauses at the gate. Either way the governance gate *fires* on every wrapped tool-call.

## 4. Storage (owned: the session ledger only)

Local SQLite, dual-write JSON-first (degrade to json-only), `PRAGMA user_version` migrations — the collection convention (REUSE-MAP §5). Everything here is a **projection** with pointers back to the authoritative sibling; nothing here is the source of truth.

```sql
-- sessions: one row per `wicked-agent run`; the workflow correlation lives in orchestration
sessions(session_id TEXT PK, workflow_id TEXT,        -- workflow_id = orchestration correlationId
         repos TEXT, problem TEXT,
         entity_mode TEXT,                              -- 'shared' | 'isolated'  (ADR-0001)
         collection_scope TEXT, vault_scope TEXT,       -- shared scope id, or per-CLI when isolated
         status TEXT,                                   -- planning|distributing|executing|completed|halted
         created_at TEXT, updated_at TEXT)
CREATE INDEX idx_sess_status ON sessions(status);

-- wrapped_clis: which CLIs this session wrapped + council's capability matrix (council-owned source)
wrapped_clis(session_id TEXT, cli_id TEXT,             -- e.g. 'claude','gemini'
             probe_ref TEXT,                            -- pointer to council's probe result (authoritative)
             supports_skills INT, supports_hook INT, supports_env INT,  -- capability matrix
             governance_mode TEXT,                      -- 'pretool-hook' | 'post-hoc'  (ADR-0003)
             scope TEXT,                                 -- collection scope this CLI writes to
             PRIMARY KEY (session_id, cli_id))

-- work_units: the plan's units; each maps to an orchestration phase + a council verdict + vault evidence
work_units(unit_id TEXT PK, session_id TEXT, ord INT,  -- topological order within the session
           description TEXT,
           assigned_cli TEXT,                           -- chosen via wicked.council.voted (council-owned)
           council_verdict_ref TEXT,                    -- pointer to the verdict
           phase_ref TEXT,                              -- pointer to the orchestration phase
           conformance_ref TEXT,                        -- pointer to governance ConformanceClaim, if gated
           evidence_id TEXT,                            -- pointer into wicked-vault
           status TEXT,                                 -- pending|running|gated|done|rejected
           updated_at TEXT)
CREATE INDEX idx_unit_session ON work_units(session_id, ord);

-- meta
meta(k TEXT PK, v TEXT)   -- schema_version, last_event_seen (bus cursor)
```

Every `*_ref` column is a pointer into a sibling's authoritative store — the ledger never duplicates a CLI registry, an event, a policy decision, or evidence.

## 5. How rigor is forced on a wrapped CLI

This is the heart of the design. wicked-agent does not trust a CLI to be rigorous; it **constructs the environment** so rigor is unavoidable, then degrades honestly where a CLI can't support a mechanism. Most agent CLIs expose three injection points — a skills/extensions directory, lifecycle hooks, and environment variables — which `lib/inject.mjs` writes per CLI before launch:

| # | Injection | Mechanism | If the CLI can't support it (degrade) |
|---|-----------|-----------|----------------------------------------|
| (a) | **wicked skills available** | write the wicked `skills/` (governance, vault, brain, testing, council) into the CLI's skills/extensions dir | skills offered as a tool manifest / prompt preamble instead |
| (b) | **governance `PreToolUse` hook** | wire `wicked-governance-call evaluate` as the CLI's pre-tool hook → `deny` blocks, `allow_with_conditions` injects obligations | **post-hoc governance** (ADR-0003): tool-calls are captured and evaluated *after* the fact; violations halt the phase and are recorded — the gate still fires, later |
| (c) | **orchestration phase events** | wicked-agent brackets the unit of work with `wicked.phase.started` … `.ready-for-gate` around the CLI subprocess | n/a — bracketing is external to the CLI; always available |
| (d) | **outputs → shared collection** | capture the CLI's artifacts/learnings into the session's collection scope (memory + knowledge + overlay) | capture stdout/diffs post-run and ingest; lower fidelity but still shared |
| (e) | **evidence → vault** | record work artifacts through `wicked-vault` (REUSE-MAP §3.1 record/verify) | n/a — recording is external; always available |

**Discovery is council's, reused** (REUSE-MAP §4 / ADR-0001) — wicked-agent never re-implements CLI detection or usability probing. The **injection-capability** dimension (does this CLI take a skills dir / a `PreToolUse` hook / env?) is the harness-specific layer wicked-agent adds on top (§2), since council's usability probe answers "can it vote," not "can I wire my hook into it." That injection dimension decides each CLI's `governance_mode` (`pretool-hook` vs `post-hoc`). The invariant — stated honestly to the user — is that **the governance gate always fires; only its timing varies** (pre-tool for capable CLIs, post-hoc for the rest). Engagement level changes the reaction, never whether the gate fires (REUSE-MAP §6.4). See ADR-0003.

## 6. The multi-CLI shared-state model (single entity vs separate sessions)

What makes N CLIs **one entity** is not the harness — it's that they read and write the **same collection scope** (REUSE-MAP §3.3). The toggle is a scope decision, owned by `lib/scope.mjs` (ADR-0001):

- **`--entity shared` (default).** All wrapped CLIs write to **one** `collection_scope`. CLI-B recalls what CLI-A learned (shared `wicked-memory`), cites the same facts (shared `wicked-knowledge`), and sees the same code-graph (shared `wicked-overlay`). They are one autonomous worker with many hands.
- **`--entity isolated`.** Each CLI gets its **own** scope. They run as separate sessions that happen to share a plan; no cross-contamination of memory. Useful for adversarial/independent passes (e.g. two CLIs solving the same unit for comparison).

This rides `wicked-memory`'s **hierarchical scope isolation** (org / unit / agent) — "shared" pins all CLIs to the *same* unit scope; "isolated" gives each its own agent scope under the session. wicked-agent owns the *toggle and the scope assignment*; the isolation mechanism itself is reused, not built (REUSE-MAP §4: "don't build a third storage layer").

## 7. Surface — skills, agents, hooks

**Skills** (`skills/<skill>/SKILL.md`), each dispatches to `wicked-agent-call`:
- `agent:run` — the full interactive flow (repos→CLIs→problem→plan→distribute→execute).
- `agent:plan` — decompose a stated problem into ordered units of work; emit `wicked.agent.plan.created`.
- `agent:distribute` — ask council per unit, consume `wicked.council.voted`, assign; emit `wicked.agent.work.distributed`.
- `agent:execute` — run a unit as an orchestration phase with the wrapped CLI injected; record evidence; emit `wicked.agent.task.completed`.
- `agent:status` — read the session ledger (which CLIs, which units, which scope, which gates).

**Agents** (`agents/*.md`, 3-agent isolation where judgment is involved — REUSE-MAP §5):
- `session-planner` — turns the problem + repos into an ordered unit-of-work plan (writer). Does **not** decide CLI assignment — that's council's job, consumed not re-derived.
- `harness-supervisor` — drives the session loop, applies per-CLI injection, brackets phases, halts on `wicked.policy.violated`. Orchestrates; does not judge its own work.
- (Conformance/evidence judgment is **not** wicked-agent's — it delegates to governance's `conformance-attester` and vault's independent attest. Evaluator ≠ creator, reused.)

**Hook mode** (the "force rigor" path, §5): wicked-agent's role is to *wire governance's hook into each wrapped CLI*, not to be a hook itself. The decision engine is governance's; wicked-agent is the installer + the post-hoc fallback when a CLI can't take the hook.

## 8. Seams (only these touch the outside)

- **Council client** — `probe(cli)` → capability matrix (reused, not re-implemented); `requestDistribution(unit)` → consume `wicked.council.voted`. Degrade: if council is absent, fall back to single-CLI, no ranking, stated honestly.
- **Orchestration client** — start a workflow per session; open/close a phase per unit of work; read `wicked.phase.*`. Degrade: if orchestration is absent, run units sequentially with local gating (no event backbone) — a material capability cut, surfaced.
- **Governance hook injection** — write `wicked-governance-call evaluate` as each capable CLI's `PreToolUse` hook; for incapable CLIs, post-hoc `evaluate` over captured tool-calls (ADR-0003). Degrade: if governance is absent, no rigor gate — wicked-agent **refuses to claim rigor** and says so (it does not silently run ungoverned).
- **Collection client** — assign + write to the session's `collection_scope` (memory/knowledge/overlay). The shared-vs-isolated toggle is a scope choice here (§6). Degrade: if the collection is absent, CLIs cannot share state → `--entity shared` is unavailable; only `isolated` (effectively independent) runs, stated.
- **EvidencePort / vault** — record work artifacts (REUSE-MAP §3.1). Degrade: evidence pending, work still runs.
- **Bus** — emit `wicked.agent.session.*`; consume `wicked.phase.*`, `wicked.council.voted`, `wicked.policy.violated`. Fire-and-forget; no-op if absent.

## 9. Open questions (honest)

1. **Capability variance across CLIs** (the dominant risk). The whole "force rigor" mechanism assumes a CLI exposes a skills dir + a pre-tool hook + env. Real CLIs differ widely; some offer none. Mitigation: council's probe → per-CLI `governance_mode`, with post-hoc governance as the floor (ADR-0003). *Falsifier:* if a meaningful class of CLIs supports *neither* a pre-tool hook *nor* reliable post-hoc tool-call capture, "same rigor regardless of which CLI runs" is false for them and must be stated as a hard limitation, not papered over.
2. **wicked-agent vs command_iq overlap** (REUSE-MAP §6.3). command_iq is a production event-sourced multi-agent platform; we chose **pattern, not dependency** (CLI harness, learn the foundry/build-floor shape — ADR-0002). Risk of slowly re-deriving solved problems. Revisit deeper reuse after the exemplar proves out.
3. **Autonomy / safety bounds.** A swarm of wrapped CLIs writing to shared state and executing in repos is powerful and dangerous. Engagement level (auto vs ask-human) gates the *reaction* at each phase gate, and governance gates each tool-call — but the blast radius of an `auto`, `--entity shared` swarm needs explicit bounds (max units in flight, repo write-scoping, a kill path on `wicked.policy.violated`). Sketched, not specified; needs an ADR before any autonomous default.
4. **Shared-state coherence under concurrency.** If two CLIs in `--entity shared` write the collection scope at once, memory/knowledge writes can race. Relies on the collection's own write semantics; wicked-agent must not assume linearizability. Needs a worked concurrency spec + antagonist tests (interleaved writes, conflicting knowledge claims).
5. **Who owns the phase gate verdict?** Inherited from REUSE-MAP §6.5: orchestration owns phase state, governance owns conformance. For a wicked-agent unit of work the gate is `orchestration phase × governance conformance`. Whether a `reject` conformance auto-rejects the phase is orchestration's ADR to settle; wicked-agent consumes the outcome, it doesn't decide precedence.
