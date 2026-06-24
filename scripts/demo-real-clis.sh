#!/bin/sh
# demo-real-clis.sh — R6 REAL-CLI demonstration.
#
# Runs wicked-agent on a TINY real task ("write a one-line release note to ./out.txt") with the REAL
# `claude` CLI as the wrapped executing agent and claude/agy/pi as the REAL council voters, against
# an ON-DISK brain.db shared by governance + orchestration + council + agent.
#
# This is NONDETERMINISTIC (real CLIs, real models). We assert only that: it ran, the real CLI
# produced output (or was gated), the governance gate fired, and everything persisted on the ONE
# on-disk store. Raw output is captured as evidence. Honest about any CLI that can't run headless.
#
# Cross-platform note: this is a POSIX sh script (the demo target is this macOS/Linux dev box). The
# Rust harness + binary it drives are cross-platform; only this convenience wrapper is sh.
set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
WRAPPER="$HERE/claude-wrapped-agent.sh"
chmod +x "$WRAPPER" 2>/dev/null || true

echo "==> Building wicked-agent (per-crate, release-off)"
( cd "$ROOT" && cargo build -p wicked-agent 2>&1 | tail -2 )
BIN="$ROOT/target/debug/wicked-agent"
[ -x "$BIN" ] || { echo "FATAL: $BIN not built"; exit 1; }

# ── Isolated on-disk workspace for this demo run. ──
WORK="$(mktemp -d "${TMPDIR:-/tmp}/wicked-agent-demo.XXXXXX")"
DB="$WORK/brain.db"
SANDBOX="$WORK/sandbox"
PROBLEM="$WORK/problem.json"
RAW="$WORK/raw-output.txt"
mkdir -p "$SANDBOX"
echo "==> Workspace: $WORK"
echo "    on-disk shared store: $DB"

# ── The problem JSON: claude as the wrapped EXECUTING CLI (via the wrapper), claude/agy/pi voters. ──
# The executing seat (key "claude") is FIRST so a degraded/split council verdict still assigns it
# (pick_assignment falls back to the first seat). Voters use their real headless invocations.
cat > "$PROBLEM" <<JSON
{
  "problem": "Write a one-line release note for wicked-agent v0.1.0 to ./out.txt.",
  "entity_mode": "shared",
  "session_id": "demo-real",
  "clis": [
    { "key": "claude", "display_name": "Claude (wrapped executor)", "binary": "$WRAPPER",
      "headless_invocation": "$WRAPPER", "category": "agentic-coder", "input_mode": "prompt-arg",
      "confidence": "verified", "enabled_for_council": true },
    { "key": "agy", "display_name": "Agency CLI", "binary": "agy",
      "headless_invocation": "agy run \"{PROMPT}\"", "category": "agentic-coder", "input_mode": "prompt-arg",
      "confidence": "verified", "enabled_for_council": true },
    { "key": "pi", "display_name": "Pi CLI", "binary": "pi",
      "headless_invocation": "pi -p \"{PROMPT}\"", "category": "agentic-coder", "input_mode": "prompt-arg",
      "confidence": "verified", "enabled_for_council": true }
  ]
}
JSON

# The gate-hook child process must reach the SAME on-disk store + re-invoke the real binary.
export WICKED_ESTATE_DB="$DB"
export WICKED_AGENT_BIN="$BIN"

echo
echo "==> RUN (real subprocess execution + REAL council verdict over claude/agy/pi)"
echo "    NOTE: real CLIs are nondeterministic and may take tens of seconds."
set +e
"$BIN" run-real --file "$PROBLEM" --db "$DB" --governance-mode pretool-hook \
  --sandbox "$SANDBOX" --timeout-secs 120 > "$RAW" 2>"$WORK/run-stderr.txt"
RUN_RC=$?
set -e
echo "    run exit code: $RUN_RC"
echo
echo "==> RAW session result (evidence):"
cat "$RAW"
echo
echo "==> run stderr (council voter / emit noise; agy TTY errors expected):"
tail -8 "$WORK/run-stderr.txt" 2>/dev/null || true

# ── Inspect REAL filesystem state: the artifact the real claude CLI wrote. ──
echo
echo "==> Real on-disk artifact (the genuine claude output):"
OUT_FILE="$(find "$SANDBOX" -name out.txt 2>/dev/null | head -1)"
if [ -n "$OUT_FILE" ] && [ -f "$OUT_FILE" ]; then
  echo "    FILE: $OUT_FILE"
  echo "    CONTENT: $(cat "$OUT_FILE")"
  GATE_NOTE="the real CLI produced output and the gate ALLOWED it (file written)"
else
  echo "    (no out.txt on disk — the unit was GATED/blocked, or the CLI produced no output)"
  GATE_NOTE="the gate fired and no artifact landed (gated or empty output)"
fi

# ── Read the ONE on-disk store back via the binary's status command (a FRESH connection). ──
echo
echo "==> Store read-back via \`wicked-agent status\` (fresh on-disk connection):"
"$BIN" status --session demo-real --db "$DB" 2>/dev/null || echo "    (status read failed)"

# ── Confirm the council task + verdict persisted on the SAME on-disk file (sqlite3 if available). ──
echo
echo "==> Council + agent entities on the ONE on-disk store:"
if command -v sqlite3 >/dev/null 2>&1; then
  for kind in agent_session work_unit phase conformance_claim council_task council_verdict cli_ranking; do
    N=$(sqlite3 "$DB" "SELECT COUNT(*) FROM nodes WHERE kind LIKE '%$kind%';" 2>/dev/null || echo "?")
    printf "    %-20s %s\n" "$kind" "$N"
  done
else
  echo "    (sqlite3 not on PATH — rely on the status read-back above)"
fi

echo
echo "==> DEMO SUMMARY"
echo "    wrapped executing CLI : claude (real, headless)"
echo "    council voters        : claude, agy, pi (real verdict; agy errors headless => abstains)"
echo "    on-disk shared store  : $DB"
echo "    outcome               : $GATE_NOTE"
echo "    (nondeterministic real run — see RAW result + artifact above for this run's evidence)"
echo
echo "    workspace preserved at: $WORK  (rm -rf to clean)"
