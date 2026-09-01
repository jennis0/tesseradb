import type {Meta, ViewInfo, ViewMetadataValue} from './types.js';

/**
 * The views of one group, in the order the server lists them (`views.md` §3.2).
 *
 * The order is the server's: `/v1/meta` lists a group's views in creation order and this walks
 * that list rather than sorting keys. **A key is the caller's own string
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

/**
 * Whether two groups address one key set (`views.md` §3.3) — `membersOf` in **either** direction,
 * and two `members` groups over one owner as well.
 *
 * Creating a key on the owner creates it on every sharer, so a key held in one group is a key held
 * in the other and a layout toggle can keep it.
 */
function sharesKeys(meta: Meta, a: string, b: string): boolean {
  if (a === b) return true;
  const ga = meta.groups.find((g) => g.name === a);
  const gb = meta.groups.find((g) => g.name === b);
  if (!ga || !gb) return false;
  return ga.membersOf === b || gb.membersOf === a || (ga.membersOf !== null && ga.membersOf === gb.membersOf);
}

/**
 * The roster metadata a view's label is read from: its own where it has any, else — for a
 * `members` group, whose views carry none of their own (`views.md` §3.3) — the owning group's view
 * under the same key, found through `membersOf`.
 */
function rosterMetadata(meta: Meta, view: ViewInfo): Record<string, ViewMetadataValue> {
  const roster = view.roster;
  if (!roster) return {};
  if (Object.keys(roster.metadata).length > 0) return roster.metadata;
  const group = meta.groups.find((g) => g.name === roster.group);
  if (!group?.membersOf) return {};
  const owner = meta.views.find((v) => v.roster?.group === group.membersOf && v.roster.key === roster.key);
  return owner?.roster?.metadata ?? {};
}

/**
 * A `timestamp_us` roster value as a date, and a `starts`/`ends` pair as a range — `en-GB`, short
 * month, **in UTC**.
 *
 * Microseconds since the epoch is the one unit that type may hold, so nothing here guesses whether
 * a large integer is a count or an instant. The zone is UTC rather than the reader's, so a quarter
 * boundary is drawn where the deployment put it and two viewers reading one roster read one label.
 *
 * Two elisions, each firing only where it is exactly true: a range whose endpoints are the first
 * and last day of a month is drawn by its months (`Jul – Sep 2026`, not `1 Jul – 30 Sep 2026`),
 * and a year shared by both endpoints is written once. Both are what makes a roster label fit a
 * 336px control beside its key.
 */
function dateSpan(startsUs: number, endsUs: number | null): string {
  const parts = (over: Intl.DateTimeFormatOptions) => new Intl.DateTimeFormat('en-GB', {timeZone: 'UTC', ...over});
  const full = parts({day: 'numeric', month: 'short', year: 'numeric'});
  const start = new Date(startsUs / 1000);
  if (endsUs === null) return full.format(start);
  const end = new Date(endsUs / 1000);
  const lastOfMonth = new Date(Date.UTC(end.getUTCFullYear(), end.getUTCMonth() + 1, 0)).getUTCDate();
  const wholeMonths = start.getUTCDate() === 1 && end.getUTCDate() === lastOfMonth;
  const sameYear = start.getUTCFullYear() === end.getUTCFullYear();
  const from = wholeMonths ? parts(sameYear ? {month: 'short'} : {month: 'short', year: 'numeric'}) : sameYear ? parts({day: 'numeric', month: 'short'}) : full;
  const to = wholeMonths ? parts({month: 'short', year: 'numeric'}) : full;
  return `${from.format(start)} – ${to.format(end)}`;
}

/**
 * What one view of a roster reads as (`view-switching.md` §6.2): its label where the group declared
 * metadata a label can be made of, and its key, which is the view's only address.
 *
 * **The rule reads metadata names by convention**, which is the only interpretation available to a
 * client, and is stated here rather than in a picker so two hosts agree: a text-typed value named
 * `label` or `title`; else a `timestamp_us`-typed `starts`, as a date and, with an `ends`, as a
 * range; else no label, and the key stands for the view. A `members` group's views carry no
 * metadata of their own and resolve through `membersOf` to the owning group's view under the same
 * key.
 *
 * A plain view is in no roster: both fields are `null`, and a picker draws its `displayName`.
 */
export function viewLabel(meta: Meta, view: ViewInfo): {label: string | null; key: string | null} {
  const roster = view.roster;
  if (!roster) return {label: null, key: null};
  const md = rosterMetadata(meta, view);
  const text = (name: string) => {
    const value = md[name];
    return value?.type === 'text' ? value.value : null;
  };
  const instant = (name: string) => {
    const value = md[name];
    return value?.type === 'timestamp_us' ? value.value : null;
  };
  const starts = instant('starts');
  return {label: text('label') ?? text('title') ?? (starts === null ? null : dateSpan(starts, instant('ends'))), key: roster.key};
}

/** One entry of the layout picker — a plain view or a whole group. See {@link viewPickerEntries}. */
export type ViewPickerEntry = {
  kind: 'view' | 'group';
  /** A view's id, or a group's name — what {@link enterGroup} and `setCurrentView` are given. */
  id: string;
  /** What the entry reads as: a view's `displayName`, a group's `title` else its `name`. */
  text: string;
  /** Whether `currentId` is this entry — the view itself, or any view of this group. */
  current: boolean;
};

/**
 * The layout picker's entries (`view-switching.md` §6.1): one per **plain view**, then one per
 * **group**, in `/v1/meta`'s serving order.
 *
 * An owner group and the `members` group laid over its keys are two entries, because they are two
 * layouts of one roster — an embedding and a map of the same quarters — and choosing between them
 * is the layout toggle. A group is one entry however many views it holds: which of them is entered
 * is {@link enterGroup}'s question, not this one's.
 */
export function viewPickerEntries(meta: Meta, currentId: string): ViewPickerEntry[] {
  const current = meta.views.find((v) => v.id === currentId) ?? null;
  const plain: ViewPickerEntry[] = meta.views
    .filter((v) => v.roster === null)
    .map((v) => ({kind: 'view', id: v.id, text: v.displayName, current: v.id === currentId}));
  const groups: ViewPickerEntry[] = meta.groups.map((g) => ({
    kind: 'group',
    id: g.name,
    text: g.title ?? g.name,
    current: current?.roster?.group === g.name
  }));
  return [...plain, ...groups];
}

/**
 * Whether a bundle offers **one layout**, and the layout picker should therefore draw nothing —
 * not an empty select (`view-switching.md` §6.1). One plain view and no groups is every demo
 * corpus today, and the toolbar looks exactly as it did before views existed.
 *
 * It is a question about layouts and not about views: a bundle whose one layout is a *group* of
 * forty quarters answers `true` here, and the key picker still draws its roster. Nothing about
 * this says a viewer has one view to look at.
 */
export function hasOneLayout(meta: Meta): boolean {
  return viewPickerEntries(meta, '').length <= 1;
}

/**
 * Which view of `group` a picker enters (`view-switching.md` §6.1), or `null` for a group with no
 * views this session may reach.
 *
 * Three rungs, in order: **the key the user is already on**, where the current view is in a group
 * sharing that key set (`membersOf` either way) — so a layout toggle keeps the quarter; else
 * `lastLeft`, the key the caller last left this group on, which is a picker's own memory and not
 * a fact about the bundle; else the group's **first view in creation order**, never its first key
 * by sort (`views.md` §3.2, decision 0113).
 */
export function enterGroup(meta: Meta, group: string, currentId: string, lastLeft?: string): string | null {
  const roster = viewsOfGroup(meta, group);
  if (roster.length === 0) return null;
  const held = meta.views.find((v) => v.id === currentId)?.roster ?? null;
  if (held && sharesKeys(meta, held.group, group)) {
    const shared = roster.find((v) => v.roster?.key === held.key);
    if (shared) return shared.id;
  }
  const remembered = lastLeft === undefined ? undefined : roster.find((v) => v.roster?.key === lastLeft);
  return (remembered ?? roster[0]!).id;
}
