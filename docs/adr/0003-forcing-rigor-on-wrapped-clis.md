# ADR-0003 — Rigor is forced by injecting governance into each wrapped CLI; the gate always fires

**Status:** Accepted (design). **Date:** 2026-06-23.

## Context

The point of wicked-agent is that wrapped CLIs work "with the SAME RIGOR we use" — not just faster, but *governed*. The honest problem: agent CLIs vary enormously in what they let an outside harness do. Some expose a `PreToolUse` (or equivalent) lifecycle hook and a skills/extensions directory and env injection; some expose only one; some expose none. If "rigor" depends on a hook the CLI doesn't have, then rigor is optional in practice — which defeats the purpose. We need a design where **the governance gate fires regardless of which CLI runs**, while being honest that the *timing and strength* of enforcement varies by CLI capability.

This builds directly on governance's own decision (governance ADR-0003): `evaluate` is deterministic and cheap — no model call on the decision path — so it is safe to run as a per-tool-call hook, and equally safe to run after the fact.

## Decision

**wicked-agent constructs each wrapped CLI's environment so governance is unavoidable, and degrades the *mechanism* — never the *fact* — of the gate based on a reused capability probe.**

1. **Probe capability with council, reused.** Before launch, council probes each CLI (REUSE-MAP §3.4): skills dir? lifecycle hook? env? wicked-agent records a `governance_mode` per CLI in its ledger.
2. **Capable CLIs get a `PreToolUse` hook.** `lib/inject.mjs` wires `wicked-governance-call evaluate` as the CLI's pre-tool hook. `deny` blocks the tool-call; `allow_with_conditions` injects obligations; `allow` proceeds. This is the strong path — enforcement *before* the action. Cheap and reproducible because governance's decision path has no model call.
3. **Incapable CLIs get post-hoc governance.** If a CLI can't take a hook, wicked-agent captures its tool-calls and runs the *same* `evaluate` immediately after each one. A violation halts the phase and is recorded. The gate still fires — just after the action, not before.
4. **The injection bundle is broader than the hook.** Capable or not, each CLI is launched with wicked skills available (a), bracketed by orchestration phase events (c), its outputs captured to the shared collection scope (d), and its artifacts recorded to vault (e). (b) — the governance hook — is the only piece whose mechanism degrades.
5. **Engagement governs the reaction, not the gate** (REUSE-MAP §6.4). An `auto` session reacts to a `deny` by halting/rerouting unattended; an `ask-human` session pauses for a human. Neither changes *whether* the gate fires.
6. **No governance present → no rigor claim.** If `wicked-governance` is absent entirely, wicked-agent refuses to claim rigor and says so plainly; it does not silently run ungoverned CLIs.

## Consequences

- ➕ "Same rigor regardless of which CLI runs" holds across heterogeneous CLIs — the *fact* of the gate is invariant; only its timing (pre-tool vs post-hoc) varies.
- ➕ Reuses governance's deterministic, model-free `evaluate` unchanged — cheap enough for a per-tool hook and for post-hoc replay, and reproducible/attestable either way.
- ➕ Reuses council's probe — no second capability-discovery mechanism (REUSE-MAP §4).
- ➕ Honest degradation ladder (pre-tool → post-hoc → refuse-to-claim) instead of a silent best-effort.
- ➖ Post-hoc governance is strictly weaker: a destructive tool-call is detected *after* it ran. Mitigation: the phase halts and evidence is recorded, but the blast radius between action and detection is real and must be bounded (ARCHITECTURE §9.3).
- ➖ Capturing every tool-call for post-hoc CLIs depends on the CLI surfacing its tool-calls reliably; some may not, leaving a capability gap (ARCHITECTURE §9.1).
- ➖ Injecting a hook into a third-party CLI's environment is inherently CLI-specific glue; `lib/inject.mjs` will carry per-CLI adapters that need maintenance as CLIs evolve.

## Falsifier

If a meaningful class of agent CLIs supports **neither** a pre-tool hook **nor** reliable post-hoc tool-call capture, then for those CLIs the governance gate cannot be made to fire, "same rigor regardless of which CLI runs" is false, and the design must state this as a hard limitation (those CLIs are ungoverned and labelled so) rather than imply uniform rigor. Equally: if wiring governance as a wrapped CLI's hook forces governance to grow CLI-specific logic — i.e. `evaluate` can't be reused unchanged — the seam is wrong and we revisit.
