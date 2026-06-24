#!/usr/bin/env node
// A FAKE-but-REAL wrapped agent CLI (Batch E E2E fixture).
//
// This is NOT a stub: it is a real subprocess that, given a TASK, performs a REAL action
// (writes an output file) AND declares its intended tool-call so the harness's governance
// gate can evaluate it. It behaves like a well-behaved agent CLI that honors a PreToolUse
// hook: before performing each tool-call it consults $WICKED_PRETOOL_HOOK (when set), piping
// the proposed tool-call as JSON on stdin, and ABORTS the action if the hook exits non-zero
// (a governance deny). It surfaces each proposed tool-call as a JSON line on stdout so a
// post-hoc harness can evaluate them too.
//
// Task contract (read from argv[2], the task file): the file's text is the work order. To
// exercise the deny path, a task line may instruct a forbidden command, which the CLI faithfully
// PROPOSES as its tool-call (so governance sees it). A clean task proposes a Write of an output
// file containing real content.
//
// Tool-call shapes emitted (one JSON object per line):
//   {"tool_call": {"tool":"Write","path":"<abs>","content":"..."}}
//   {"tool_call": {"tool":"Bash","command":"export API_KEY=..."}}

import { spawnSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const taskFile = process.argv[2] || process.env.WICKED_TASK_FILE;
const workdir = process.env.WICKED_WORKDIR || process.cwd();
const hook = process.env.WICKED_PRETOOL_HOOK || "";

let task = "";
try {
  task = readFileSync(taskFile, "utf8");
} catch (e) {
  process.stderr.write(`fake-agent-cli: cannot read task file: ${e.message}\n`);
  process.exit(1);
}

/** Emit a proposed tool-call as a JSON line on stdout (so the harness can capture it). */
function declare(toolCall) {
  process.stdout.write(JSON.stringify({ tool_call: toolCall }) + "\n");
}

/**
 * Consult the PreToolUse hook (when wired). Returns true if allowed to proceed.
 * The hook gets the proposed tool-call on stdin; a non-zero exit = governance DENY.
 */
function preToolApproved(toolCall) {
  if (!hook) return true; // post-hoc mode: no pre-tool gate; harness evaluates after.
  const r = spawnSync(process.execPath, [hook], {
    input: JSON.stringify(toolCall),
    encoding: "utf8",
    env: process.env,
  });
  // exit 0 = allow, anything else (notably 2) = deny.
  return (r.status ?? 1) === 0;
}

// Decide what tool-call this task wants. A task mentioning a forbidden secret-export is
// faithfully proposed as a Bash command (governance will deny it); otherwise the CLI writes
// a real output file with derived content.
const wantsForbidden = /API_KEY|secret|export\s+\w+=/i.test(task);

if (wantsForbidden) {
  // The CLI's intended tool-call is the forbidden command. PROPOSE it (so governance sees it).
  const command = "export API_KEY=sk-live-DEADBEEF-secret-token";
  const toolCall = { tool: "Bash", command };
  declare(toolCall);
  // Honor the pre-tool gate: if denied, ABORT without performing the effect.
  if (!preToolApproved(toolCall)) {
    process.stderr.write("fake-agent-cli: pretool hook DENIED the command; aborting (no effect)\n");
    process.exit(2);
  }
  // (If somehow allowed) perform the effect — write a file proving the secret was handled.
  const outPath = join(workdir, "secret-leak.txt");
  writeFileSync(outPath, command + "\n", "utf8");
  process.stdout.write(`fake-agent-cli: wrote ${outPath}\n`);
  process.exit(0);
}

// Clean task: the CLI's intended tool-call is to Write a real output file.
const outPath = join(workdir, "output.txt");
const content = `done: ${task.trim()}\n`;
const toolCall = { tool: "Write", path: outPath, content };
declare(toolCall);
if (!preToolApproved(toolCall)) {
  process.stderr.write("fake-agent-cli: pretool hook DENIED the write; aborting (no effect)\n");
  process.exit(2);
}
// Perform the REAL action.
writeFileSync(outPath, content, "utf8");
process.stdout.write(`fake-agent-cli: wrote ${outPath}\n`);
process.exit(0);
