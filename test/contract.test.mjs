// wicked-agent — Phase-1 skeleton contract test (node:test, stdlib only).
//
// Proves compatibility with the locked spine (BUILD-SPINE §3.4) BEFORE any
// behavior exists:
//   (a) every declared emit/consume is a subset of the locked catalog with a
//       matching producer (assertAppConforms, reused from the spine validator);
//   (b) the shared-vs-isolated scope toggle actually differs — DIFFERENT scope
//       per CLI in "isolated", SAME scope across CLIs in "shared";
//   (c) NEGATIVE: an off-catalog consume name fails the contract.
//
// The validator is imported READ-ONLY from the governance contracts dir (the
// locked spine); this test never modifies it.

import { test } from "node:test";
import assert from "node:assert/strict";

import { assertAppConforms } from "../../wicked-governance/contracts/validate-contract.mjs";
import { DOMAIN, EMITS, CONSUMES } from "../lib/events.mjs";
import { resolveScope } from "../lib/scope.mjs";

// (a) declared events conform to the locked catalog.
test("declared emits/consumes conform to the locked catalog", () => {
  const r = assertAppConforms({ domain: DOMAIN, emits: EMITS, consumes: CONSUMES });
  assert.equal(r.ok, true, `contract violations: ${r.errors.join("; ")}`);
});

test("domain is wicked-agent and emits the agent.session vocabulary", () => {
  assert.equal(DOMAIN, "wicked-agent");
  assert.ok(EMITS.includes("wicked.agent.session.started"));
  assert.ok(EMITS.includes("wicked.agent.session.completed"));
});

// (b) the single-entity-vs-separate toggle (ARCHITECTURE §6) must actually differ.
test("isolated mode gives a DIFFERENT scope per CLI", () => {
  const a = resolveScope({ entityMode: "isolated", sessionId: "s1", cliId: "claude" });
  const b = resolveScope({ entityMode: "isolated", sessionId: "s1", cliId: "gemini" });
  assert.equal(a.shared, false);
  assert.equal(b.shared, false);
  assert.notEqual(a.scope, b.scope, "isolated CLIs must not share a scope");
});

test("shared mode gives the SAME scope across CLIs", () => {
  const a = resolveScope({ entityMode: "shared", sessionId: "s1", cliId: "claude" });
  const b = resolveScope({ entityMode: "shared", sessionId: "s1", cliId: "gemini" });
  assert.equal(a.shared, true);
  assert.equal(b.shared, true);
  assert.equal(a.scope, b.scope, "shared CLIs must write to one entity scope");
});

test("shared and isolated resolve to different scopes for the same CLI", () => {
  const shared = resolveScope({ entityMode: "shared", sessionId: "s1", cliId: "claude" });
  const isolated = resolveScope({ entityMode: "isolated", sessionId: "s1", cliId: "claude" });
  assert.notEqual(shared.scope, isolated.scope);
});

// (c) NEGATIVE: an off-catalog consume name must fail the contract.
test("off-catalog consume fails the contract (negative pair)", () => {
  const r = assertAppConforms({
    domain: DOMAIN,
    emits: EMITS,
    consumes: [...CONSUMES, "wicked.bogus.event"],
  });
  assert.equal(r.ok, false, "off-catalog consume should have failed");
  assert.ok(
    r.errors.some((e) => e.includes("wicked.bogus.event")),
    `expected an error naming the off-catalog event, got: ${r.errors.join("; ")}`
  );
});

// NEGATIVE: emitting an event this app does not produce must fail.
test("emitting a non-producer event fails the contract (negative pair)", () => {
  const r = assertAppConforms({
    domain: DOMAIN,
    emits: [...EMITS, "wicked.council.voted"], // council's event, not agent's
    consumes: CONSUMES,
  });
  assert.equal(r.ok, false, "emitting another app's event should have failed");
});
