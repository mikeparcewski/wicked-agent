# ADR-0001 — The harness wraps many CLIs; shared collection scope makes them one entity

> **Status: Implemented (2026-06-24)** — Rust harness drives the three in-process on one shared estate store; shared/isolated is a collection-scope toggle.

**Status:** Accepted (design). **Date:** 2026-06-23.

## Context

wicked-agent must "run MULTIPLE agent CLIs that share their details and operate as a SINGLE entity (or as separate sessions if the user wants)." Two design questions fall out: (1) where does the harness get the list of CLIs and their capabilities, and (2) what actually makes N independent CLI processes behave as *one* worker rather than N divergent ones — and how does the user flip between "one entity" and "separate sessions"?

The naive path is for wicked-agent to discover CLIs itself and to invent a shared-state store. Both are wrong: `wicked-council` already discovers and probes real CLIs (REUSE-MAP §3.4), and the **collection** already provides shared memory/knowledge/code-graph with hierarchical scope isolation (REUSE-MAP §3.3). The collection rule holds: don't build a third (here, fourth) storage layer; reuse where it's smart.

## Decision

**The harness wraps CLIs it does not discover, and shared state is a collection-scope decision it does not store.**

1. **CLI discovery/probe is council's, reused.** wicked-agent asks council which CLIs exist and probes each for injection capability (skills dir? hook? env?). wicked-agent keeps only a derived capability matrix in its session ledger (`wrapped_clis`), pointing back to council's authoritative probe result.
2. **"One entity" = one shared collection scope.** All wrapped CLIs in a session read/write the *same* `collection_scope`. CLI-B recalls CLI-A's memory, cites the same knowledge, sees the same overlay code-graph. The harness assigns the scope; the collection enforces the sharing.
3. **Single-entity vs separate sessions is a scope toggle.** `--entity shared` (default) pins all CLIs to one unit scope; `--entity isolated` gives each CLI its own agent scope. This rides `wicked-memory`'s existing org/unit/agent hierarchical isolation — wicked-agent owns the *toggle*, not the isolation mechanism.
4. **The session ledger is a projection, not a source of truth.** Sessions/wrapped-clis/work-units hold pointers into council (rankings), orchestration (phases/events), governance (conformance), and vault (evidence). Lose the ledger and the authoritative record survives in the siblings.

## Consequences

- ➕ No re-implemented CLI registry, no new shared-state store — wicked-agent is integration, not new primitives (REUSE-MAP §4).
- ➕ "Single entity or separate sessions" is a one-flag config, not two code paths, because it reduces to *which scope CLIs write to*.
- ➕ Shared learning compounds: in `--entity shared`, every CLI's output enriches the memory/knowledge the next CLI recalls.
- ➕ The ledger can be rebuilt from sibling state; wicked-agent stays thin and disposable.
- ➖ Hard dependency on the collection for the headline "one entity" feature. Cold-start (empty scope) means early CLIs share little until learnings accumulate — stated honestly to callers.
- ➖ `--entity shared` exposes the harness to the collection's concurrency semantics (interleaved writes from parallel CLIs). wicked-agent must not assume linearizability (ARCHITECTURE §9.4).

## Falsifier

If wrapping a *second* CLI does not let the two CLIs share state and act as one entity — i.e. CLI-B cannot recall what CLI-A learned in the same session via the shared collection scope, and each re-discovers/re-decides in isolation — then wicked-agent is just a process launcher, the "single entity" claim is false, and the design is wrong. Equivalently: if making them "one entity" requires wicked-agent to build its own shared store rather than reuse a collection scope, we've violated the reuse map and must revisit.
