```
          _      _            _                              _   
__      _(_) ___| | _____  __| |       __ _  __ _  ___ _ __ | |_ 
\ \ /\ / / |/ __| |/ / _ \/ _` |_____ / _` |/ _` |/ _ \ '_ \| __|
 \ V  V /| | (__|   <  __/ (_| |_____| (_| | (_| |  __/ | | | |_ 
  \_/\_/ |_|\___|_|\_\___|\__,_|      \__,_|\__, |\___|_| |_|\__|
                                            |___/                
```

**The harness that turns one or many agent CLIs into a single governed entity.** Point it at your repos, pick the agent CLIs you trust, state the problem — and it plans the work, distributes it with [`wicked-council`](../wicked-council), runs each unit as a [`wicked-orchestration`](../wicked-orchestration) phase, and forces every wrapped CLI to act with the *same rigor* the estate uses: a [`wicked-governance`](../wicked-governance) gate on the work, shared state in the collection, evidence for every claim.

> **Status:** v0.4.0 · `cargo test` **47 passed** (44 lib + 3 cogiq-bench) `clippy -D warnings` clean, plus 4 live `--ignored` e2e (real-claude deny-gate; human-confirm + resume + MCP-toolbox in one governed session; memory/knowledge MCP serve proofs). A **real** run has `claude` (headless) write real output under the governance gate — now with a typed **human-confirm gate** (`--human-confirm none|all|before:<N>` + a `resume` subcommand) and **MCP toolbox injection** (augment-mode `--mcp-config`: the wicked-estate / wicked-memory / wicked-knowledge servers) — distributed by a real council verdict over `claude`/`agy`/`pi`, on an on-disk shared store. (`agy` is not headless in all envs → it abstains as a voter.) Part of the **wicked-estate universe** (polyrepo — one product per repo); sibling crates are pinned to published versions.

## Architecture (drives the three in-process)
- Opens **one** shared estate store (the collection) and drives governance + orchestration + council as in-process library crates: `plan → distribute (real council verdict) → execute → evidence`.
- **Execute** runs the assigned CLI as a real subprocess; a governance gate fires (unit-level pre-launch, and a per-tool-call hook) — a `Deny` blocks the action and the effect never lands (mutation-proved: neuter the gate and the forbidden file appears on disk).
- Session, work-units, claims, phases, outputs all persist on the shared store; shared-vs-isolated entity is a collection-scope toggle.

See [`ARCHITECTURE.md`](ARCHITECTURE.md), [`docs/adr/`](docs/adr/), and `scripts/`.

## Build
```sh
cargo test                                  # 17 passed
cargo clippy --all-targets -- -D warnings
bash scripts/demo-real-clis.sh              # the real-CLI run (claude + council over claude/agy/pi)
```

## License
MIT.
