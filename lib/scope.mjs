// wicked-agent — the single-entity-vs-separate toggle (ARCHITECTURE §6).
//
// What makes N wrapped CLIs ONE entity is not the harness — it is that they
// read and write the SAME collection scope. The toggle is a scope decision,
// owned here. This rides wicked-memory's hierarchical scope isolation
// (org / unit / agent); wicked-agent owns the toggle + the scope assignment,
// the isolation mechanism itself is reused, not built (ADR-0001).
//
// Phase-1: stub logic — no collection client is wired yet. But the
// shared/isolated branch MUST differ, because that difference is the whole
// design claim the skeleton has to keep honest:
//   - "shared":   every CLI gets the SAME scope id  -> one entity, many hands
//   - "isolated": every CLI gets its OWN scope id    -> independent sessions

/**
 * Resolve the collection (and vault) scope a wrapped CLI should write to.
 *
 * @param {object}  args
 * @param {"shared"|"isolated"} args.entityMode  scope mode (ARCHITECTURE §6).
 * @param {string}  args.sessionId               the wicked-agent session id.
 * @param {string}  args.cliId                   the wrapped CLI, e.g. "claude".
 * @returns {{ mode: string, sessionId: string, cliId: string, scope: string, shared: boolean }}
 */
export function resolveScope({ entityMode, sessionId, cliId } = {}) {
  if (!sessionId) throw new Error("resolveScope: sessionId is required");
  if (!cliId) throw new Error("resolveScope: cliId is required");

  const mode = entityMode === "isolated" ? "isolated" : "shared";

  // shared  -> pin ALL CLIs to one unit scope under the session (cliId omitted).
  // isolated -> give EACH CLI its own agent scope under the session.
  const scope =
    mode === "shared"
      ? `wicked-agent/${sessionId}/shared`
      : `wicked-agent/${sessionId}/cli/${cliId}`;

  return { mode, sessionId, cliId, scope, shared: mode === "shared" };
}
