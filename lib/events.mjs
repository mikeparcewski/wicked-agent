// wicked-agent — bus event declaration (Phase-1 skeleton).
//
// This module is the single source of truth for which catalog events this app
// produces and consumes. The contract test (test/contract.test.mjs) asserts
// every name here is a subset of the locked catalog
// (../../wicked-governance/contracts/events.json) via assertAppConforms.
//
// Convention: wicked.<noun>.<verb>. Do NOT add a name that is not in the
// catalog — an off-catalog name fails the contract test by design.

export const DOMAIN = "wicked-agent";

// Producer events — wicked-agent is the catalog `producer` of each of these.
// The session vocabulary (REUSE-MAP §3.2 / BUILD-SPINE §3.1).
export const EMITS = [
  "wicked.agent.session.started",
  "wicked.agent.plan.created",
  "wicked.agent.work.distributed",
  "wicked.agent.task.completed",
  "wicked.agent.session.completed",
];

// Consumer events — wicked-agent subscribes to these (it appears in the
// catalog `consumers` list for each). The harness consumes orchestration's
// phase lifecycle, council's verdict, and governance's violation signal
// (ARCHITECTURE §3, §8). Every name below is verified present in
// events.json with wicked-agent listed as a consumer.
export const CONSUMES = [
  "wicked.phase.started",
  "wicked.phase.ready-for-gate",
  "wicked.phase.approved",
  "wicked.phase.rejected",
  "wicked.council.voted",
  "wicked.policy.violated",
];
