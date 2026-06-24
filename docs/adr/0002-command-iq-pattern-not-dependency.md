# ADR-0002 — command_iq's foundry/build-floor is a pattern, not a dependency

> **Status: Holds (2026-06-24)** — no command_iq dependency; the event/phase model is re-expressed on the estate store.

**Status:** Accepted (design). **Date:** 2026-06-23.

## Context

wicked-agent is a harness that spawns and supervises multiple agent runtimes — exactly the shape of command_iq's **foundry / build-floor**, where an agent runtime port spawns roles and runs them as an event-sourced multi-agent platform. command_iq is a mature, production system that has already solved orchestration, role spawning, evidence lineage, and idempotency. The temptation is to depend on it directly. The countervailing fact (REUSE-MAP §6.3): command_iq is a heavyweight production platform, and the four-app set deliberately re-expresses its *ideas* in the wicked estate's thinner, agent-facing, CLI-first idiom — the same choice orchestration made for the event model (REUSE-MAP §1, "command_iq event model as pattern").

## Decision

**Reuse the foundry/build-floor *shape* as a pattern; take no code dependency on command_iq.** Concretely:

- **Borrow the shape.** An "agent runtime port that spawns roles" maps directly to wicked-agent's harness wrapping CLIs as units of work. The build-floor's "many roles, one coordinated build" maps to "many wrapped CLIs, one governed session."
- **Borrow the discipline, via the siblings, not via command_iq.** The event-sourced backbone is `wicked-orchestration` (which itself learned command_iq's commands→events→projections). Idempotency keys on every event (REUSE-MAP §3.2). Evidence lineage via the `EvidencePort → wicked-vault` seam (REUSE-MAP §3.1). wicked-agent gets these by composing the siblings — not by importing command_iq.
- **Take no dependency.** wicked-agent does not link, call, or require command_iq at runtime. If it ever needs a primitive command_iq has, it reaches for the wicked sibling that re-expresses it.

## Consequences

- ➕ wicked-agent stays in the estate's thin, CLI-first, locally-installable Node idiom (mirrors `wicked-brain`/`wicked-testing`) — no heavyweight platform runtime to stand up.
- ➕ The proven *ideas* (runtime-spawns-roles, event sourcing, evidence lineage, idempotency) are inherited through the siblings, so we don't re-derive them from scratch either.
- ➕ Clear boundary: command_iq is reference reading, not a build dependency — no version coupling, no platform lock-in.
- ➖ Risk of slowly re-deriving problems command_iq already solved deeply (REUSE-MAP §6.3 names this explicitly). Mitigation: keep command_iq as the named pattern reference and revisit deeper reuse once the capstone proves out.
- ➖ "Pattern parity" is unenforced — nothing mechanically checks that our harness shape stays faithful to the foundry model; drift is possible and only caught by review.

## Falsifier

If building the harness forces us to import, link against, or run command_iq to get a capability — i.e. composing council + orchestration + governance + the collection is *insufficient* and only command_iq's actual code closes the gap — then "pattern, not dependency" is false for that capability, and we must either deepen a sibling or accept the dependency explicitly (and amend the reuse map). Equally: if our harness shape drifts so far from foundry/build-floor that the pattern reference no longer informs it, the citation is dead weight and should be dropped.
