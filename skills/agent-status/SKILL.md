---
name: agent-status
description: Read the wicked-agent session ledger — which CLIs were wrapped, which units of work ran, which collection scope, which phase gates. Use to inspect a governed multi-CLI session. STATUS: skeleton — not implemented.
---

# agent:status

Status: skeleton — not implemented.

Reads the session ledger (ARCHITECTURE §4): sessions, wrapped CLIs + their
capability matrix, and work units with their pointers back into the siblings
(council verdict, orchestration phase, governance conformance, vault evidence).

Dispatches to `wicked-agent-call status` once implemented (Phase-2, BUILD-SPINE
§2 batch D). For now this skill is a stub; only `wicked-agent-call health` works.
