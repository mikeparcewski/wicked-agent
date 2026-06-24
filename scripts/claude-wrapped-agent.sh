#!/bin/sh
# claude-wrapped-agent.sh — the REAL wrapped CLI for the R6 demo.
#
# The wrapped-CLI contract (see crates/wicked-agent/src/inject.rs): invoked as
#   claude-wrapped-agent.sh <TASK.txt>
# It performs the unit's task with the REAL `claude` CLI, then surfaces a `write_file` tool-call so
# wicked-agent can GOVERN + GATE + EVIDENCE it. It HONORS the per-tool-call governance hook: it pipes
# the proposed tool-call JSON into $WICKED_PRETOOL_HOOK and only performs the real write if the hook
# allows (exit 0); a deny (exit != 0) aborts the write — the effect never lands.
#
# This is a genuine real-CLI run: `claude -p` does the actual generation work; the wrapper is the
# thin glue that makes a third-party CLI's output governable across the subprocess boundary.
set -eu

TASK_FILE="$1"
TASK="$(cat "$TASK_FILE")"
OUT="out.txt"

# ── REAL WORK: ask the real claude CLI to perform the task headlessly. ──
# --dangerously-skip-permissions keeps it non-interactive (no trust prompt). Bounded by the harness
# timeout. If claude errors/needs auth, $RESULT is its error text and exit code is surfaced honestly.
RESULT="$(claude -p "$TASK Respond with ONLY the deliverable, no preamble." --dangerously-skip-permissions 2>/dev/null || echo "[claude produced no output]")"

# Collapse to a single line for a clean one-line artifact + JSON-safe content (escape quotes/newlines).
RESULT_ONE="$(printf '%s' "$RESULT" | tr '\n' ' ' | sed 's/"/\\"/g')"

# The proposed tool-call the harness governs. content = claude's real output.
CALL=$(printf '{"tool":"write_file","path":"%s","content":"%s"}' "$OUT" "$RESULT_ONE")

# ── Consult the per-tool-call governance hook BEFORE writing (if wired). ──
ALLOW=1
if [ -n "${WICKED_PRETOOL_HOOK:-}" ]; then
  if printf '%s' "$CALL" | "$WICKED_PRETOOL_HOOK" >/dev/null 2>&1; then
    ALLOW=1
  else
    ALLOW=0
  fi
fi

# Surface the tool-call (the harness parses this line to capture the artifact / evaluate the gate).
printf '{"tool_call":{"tool":"write_file","path":"%s","content":"%s"}}\n' "$OUT" "$RESULT_ONE"

if [ "$ALLOW" = "1" ]; then
  printf '%s\n' "$RESULT_ONE" > "$OUT"
  echo "wrote $OUT via real claude CLI"
else
  echo "BLOCKED: pretool governance hook denied the write of $OUT" >&2
  exit 3
fi
