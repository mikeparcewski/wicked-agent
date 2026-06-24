# wicked-agent

**The harness that turns one or many agent CLIs into a single governed entity.** Point it at your repos, pick the agent CLIs you trust, state the problem — and it plans the work, distributes it with `wicked-council`, runs it as a `wicked-orchestration` workflow, and forces every wrapped CLI to act with the *same rigor* the wicked estate uses: governance on every tool-call, shared memory of what's been learned, evidence for every claim.

> **Status: design — not built.** This repo currently contains the design (`README`, `ARCHITECTURE`, `docs/adr/`). It composes the other three apps in the set — `wicked-council`, `wicked-orchestration`, `wicked-governance` — which are themselves design-stage; the shared contracts live in [`../wicked-governance/docs/REUSE-MAP.md`](../wicked-governance/docs/REUSE-MAP.md) (the spine). No code, no tests, no green to claim. The next gate is a scaffold that compiles + a two-CLI session that completes one distributed unit of work under a governance hook. Falsifier for "this design is sound": if wrapping a second agent CLI does **not** let two CLIs share state and operate as one entity — i.e. they each re-discover and re-decide in isolation — then wicked-agent is just a launcher and the design failed (see ADR-0001).

---

## Why

Agent CLIs are improving fast, but each one is an island. It discovers its own tools, holds its own context, forgets what the last run learned, and is governed only by whatever prose sits in its prompt. Run two of them on the same problem and you get two divergent opinions, twice the token burn, and no shared record. The estate already solved the *hard* parts of this in three sibling apps; what's missing is the **harness** that makes them act as one:

- **One entity, many CLIs.** Multiple wrapped CLIs share memory, knowledge, and a code-graph through the **collection** — so they operate as a single autonomous worker (or as deliberately-isolated sessions; it's a config toggle, see ADR-0001).
- **Plan, then distribute by capability.** `wicked-council` already discovers and probes real CLIs and ranks them by memory. wicked-agent reuses that to pick the best CLI for each unit of work — it does **not** re-implement CLI discovery.
- **A session is a workflow.** Each distributed unit of work is a `wicked-orchestration` phase with a gate. wicked-agent doesn't invent a state machine; it emits `wicked.agent.session.*` and consumes `wicked.phase.*`.
- **Same rigor, regardless of which CLI runs.** wicked-agent injects `wicked-governance` as a `PreToolUse` hook into each wrapped CLI's environment, so the policy gate fires no matter whose CLI is executing. Engagement (auto vs ask-human) governs the *reaction*, never whether the gate fires.
- **Every claim carries proof.** Work evidence is recorded through `wicked-vault`.

wicked-agent ships **no new primitives.** It is integration: the harness/session loop, the per-CLI injection, and the single-entity-vs-separate toggle. If a feature here looks like a new storage layer, a new event bus, or a new CLI registry, it's a bug — reuse the sibling (REUSE-MAP §4 anti-reuse).

## Where this fits

wicked-agent is the **capstone** — it sits *atop* the other three apps and the collection, consuming them; nothing in the set depends on it.

| Layer | Tool | Role |
|-------|------|------|
| **Harness / execution** | **wicked-agent** | **wrap CLIs · plan · distribute · execute under rigor · single entity or separate sessions** |
| Work distribution | `wicked-council` | discover/probe agent CLIs, rank by memory, pick the best CLI per unit of work |
| Event & phase backbone | `wicked-orchestration` | the session *is* a workflow; each unit of work is a phase with a gate |
| Rigor & policy | `wicked-governance` | `PreToolUse` gate injected into every wrapped CLI (`allow`/`deny`/obligations) |
| Shared state | collection (`wicked-memory` / `wicked-knowledge` / `wicked-overlay`) | the shared memory/knowledge/code-graph that makes many CLIs one entity |
| Evidence | `wicked-vault` | tamper-evident record of work artifacts |
| Events | `wicked-bus` | consume all sibling events; emit `wicked.agent.session.*` (fire-and-forget) |

Shared contracts live in [`../wicked-governance/docs/REUSE-MAP.md`](../wicked-governance/docs/REUSE-MAP.md). The harness *shape* (an agent runtime that spawns roles) borrows from command_iq's foundry/build-floor as a **pattern, not a dependency** (ADR-0002, REUSE-MAP §6.3).

## Install (planned)

```sh
npm i -g wicked-agent              # installs skills + the wicked-agent-call CLI
# composes wicked-council, wicked-orchestration, wicked-governance, the collection, and wicked-bus
# (each installed separately; wicked-agent degrades gracefully if a sibling is absent — see ARCHITECTURE §6)
```

## Quickstart (designed surface)

The headline experience is one interactive flow: **repos → CLIs → problem → plan → distribute → execute**. It maps 1:1 to a `wicked-orchestration` workflow.

```sh
wicked-agent run
```

```
1. Which repo(s) should I work on?           ──► repos pinned to the session scope
   > wicked-agent, wicked-governance

2. Which agent CLIs should I use?            ──► reuses council's registry/probe (no re-discovery)
   > claude, gemini            (council probes each: skills-dir? hooks? env? → capability matrix)

3. What's the problem, and what work is needed?
   > "Wire the EvidencePort vault adapter and prove it with a contract test."

4. Plan                                       ──► emits wicked.agent.session.started + .plan.created
   - [unit A] implement adapter          (council ranks: claude — best at this work-kind by memory)
   - [unit B] author contract test       (council ranks: gemini)

5. Distribute                                 ──► asks council per unit; emits .work.distributed
   - consumes wicked.council.voted → assignment per unit of work

6. Execute                                    ──► each unit is an orchestration phase with a gate
   - each wrapped CLI launched with: wicked skills installed · governance PreToolUse hook wired ·
     outputs captured to the shared collection scope · evidence recorded to vault
   - emits wicked.agent.task.completed per unit; wicked.agent.session.completed at the end
```

Two scope modes (ADR-0001):

```sh
wicked-agent run --entity shared     # default: CLIs share one collection scope → ONE entity
wicked-agent run --entity isolated   # separate sessions: each CLI gets its own scope
```

**As a skill** the agent calls `agent:run` (or the step skills `agent:plan`, `agent:distribute`, `agent:execute`). **As a CLI** the same flow runs headless for CI. Both surfaces, one harness.

## Architecture

See [`ARCHITECTURE.md`](ARCHITECTURE.md). In one breath: a Node tool (mirrors `wicked-brain`/`wicked-testing`) — `skills/` + `agents/` + `lib/*.mjs` + local SQLite for the **session loop and the wrapped-CLI/work-unit ledger** — that owns the harness and per-CLI injection and reuses *everything else*: council for distribution, orchestration for phases/events, governance for the rigor gate, the collection for shared state, vault for evidence, the bus to coordinate.

## Build / test (planned gate)

```sh
npm test                          # node:test
# gate: two-CLI shared-state session completes one unit of work ·
#       governance PreToolUse hook proven to fire inside a wrapped CLI (and degrade to post-hoc) ·
#       council-distribution + orchestration-phase integration contract tests · cross-platform
```

Nothing is "done" on a claim — prove mechanically, verify independently, cross-check (REUSE-MAP §5). Every "done" here carries an evidence path + a falsifier + what's still not done.

## Roadmap

1. Scaffold (package, `wicked-agent-call` dispatcher, session schema, skill stubs) — compiles + installs.
2. The interactive flow (repos → CLIs → problem) reusing council's registry/probe; emit `wicked.agent.session.started`/`.plan.created`.
3. Per-CLI injection: skills dir + governance `PreToolUse` hook + env, with a capability probe and graceful degradation to post-hoc governance (ADR-0003).
4. Distribution via council (`wicked.council.voted`) + execution as orchestration phases (`wicked.phase.*`); shared-collection capture of outputs.
5. Shared-vs-isolated scope toggle (ADR-0001); vault evidence per unit of work; `wicked.agent.session.completed`.

## License

MIT.
