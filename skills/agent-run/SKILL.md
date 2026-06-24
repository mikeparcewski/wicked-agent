---
name: agent-run
description: Run the full wicked-agent interactive flow — repos to CLIs to problem to plan to distribute to execute. Wraps one or many agent CLIs into a single governed entity. Use when starting a governed multi-CLI session. STATUS: skeleton — not implemented.
---

# agent:run

Status: skeleton — not implemented.

The full interactive flow (ARCHITECTURE §3): pin repos to the session scope,
name the agent CLIs (reusing council's registry/probe), capture the problem,
plan into ordered units of work, distribute each via council, and execute each
as a wicked-orchestration phase with the governance gate wired into the wrapped
CLI.

Dispatches to `wicked-agent-call run` once implemented (Phase-2, BUILD-SPINE §2
batch D). For now this skill is a stub; only `wicked-agent-call health` works.
