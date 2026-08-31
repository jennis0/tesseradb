import type {Meta, ViewInfo} from './types.js';

/**
 * The views of one group, in ordinal order (`views.md` §3.2).
 *
 * The order is the server's: `/v1/meta` lists a group's views by ordinal — creation order, never
 * reused — and this walks that list rather than sorting keys. **A key is the caller's own string
 * and means nothing to a client**: `2026-Q2` sorts after `2026-Q10` and a key need not be a date
 * at all, so a picker that sorted keys would offer a corpus's quarters in an order nobody chose.
 *
 * An unknown group is the empty list, which is the same answer as a group with no views — a
 * picker draws nothing in either case.
 */
export function viewsOfGroup(meta: Meta, group: string): ViewInfo[] {
  const byId = new Map(meta.views.map((v) => [v.id, v]));
  const found = meta.groups.find((g) => g.name === group);
  return (found?.views ?? []).flatMap((id) => {
    const view = byId.get(id);
    return view ? [view] : [];
  });
}

/**
 * The view `step` places along its own group's roster — `stepView(meta, id, -1)` and
 * `stepView(meta, id, 1)` are previous and next.
 *
 * `null` at either end, and `null` for a plain view, which is in no group and has no neighbours.
 * It does **not** wrap: a picker that wrapped would take a viewer from the last quarter back to
 * the first without saying so.
 */
export function stepView(meta: Meta, id: string, step: number): ViewInfo | null {
  const view = meta.views.find((v) => v.id === id);
  if (!view?.roster) return null;
  const siblings = viewsOfGroup(meta, view.roster.group);
  const at = siblings.findIndex((v) => v.id === id);
  return (at < 0 ? null : siblings[at + step]) ?? null;
}
