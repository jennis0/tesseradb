import type {Meta, ViewInfo, ViewMetadataValue} from './types.js';

/**
 * The views of one group, in the server's order, which is creation order. Keys are not sorted: a
 * key is the caller's own string, and `2026-Q2` sorts after `2026-Q10`. An unknown group gives the
 * empty list, as a group with no views does.
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
 * The view `step` places along its group: `-1` is previous and `1` next. `null` at either end and
 * for a plain view, which is in no group. It does not wrap.
 */
export function stepView(meta: Meta, id: string, step: number): ViewInfo | null {
  const view = meta.views.find((v) => v.id === id);
  if (!view?.roster) return null;
  const siblings = viewsOfGroup(meta, view.roster.group);
  const at = siblings.findIndex((v) => v.id === id);
  return (at < 0 ? null : siblings[at + step]) ?? null;
}

/**
 * Whether two groups address one key set: `membersOf` in either direction, or two `members` groups
 * over one owner. A key created on the owner is created on every sharer.
 */
function sharesKeys(meta: Meta, a: string, b: string): boolean {
  if (a === b) return true;
  const ga = meta.groups.find((g) => g.name === a);
  const gb = meta.groups.find((g) => g.name === b);
  if (!ga || !gb) return false;
  return ga.membersOf === b || gb.membersOf === a || (ga.membersOf !== null && ga.membersOf === gb.membersOf);
}

/**
 * The roster metadata a view's label is read from: its own where it has any, else, for a `members`
 * group whose views carry none, the owning group's view under the same key.
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
 * A `timestamp_us` roster value as a date, and a `starts`/`ends` pair as a range, in `en-GB` with a
 * short month, in UTC so every viewer reads one label. A range covering whole months is written by
 * its months (`Jul – Sep 2026`), and a year both ends share is written once.
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
 * What one view of a roster reads as: a label and its key, which is the view's only address.
 *
 * The label is a text value named `label` or `title`; else a `timestamp_us` `starts`, as a date or,
 * with an `ends`, a range; else none, and the key stands for the view. A `members` group's views
 * resolve through `membersOf` to the owner's view under the same key. A plain view is in no roster
 * and both fields are `null`.
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

/** One entry of the layout picker: a plain view or a whole group. See {@link viewPickerEntries}. */
export type ViewPickerEntry = {
  kind: 'view' | 'group';
  /** A view's id or a group's name, as {@link enterGroup} and `setCurrentView` take. */
  id: string;
  /** What the entry reads as: a view's `displayName`, a group's `title` else its `name`. */
  text: string;
  /** Whether `currentId` is this view, or any view of this group. */
  current: boolean;
};

/**
 * The layout picker's entries: one per plain view, then one per group, in `/v1/meta`'s order. An
 * owner group and a `members` group over its keys are two entries, two layouts of one roster. A
 * group is one entry however many views it holds; {@link enterGroup} picks which is entered.
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
 * Whether a bundle offers one layout, so the layout picker draws nothing. A bundle whose one layout
 * is a group of many views answers `true`, and its key picker still draws the roster.
 */
export function hasOneLayout(meta: Meta): boolean {
  return viewPickerEntries(meta, '').length <= 1;
}

/**
 * Which view of `group` a picker enters, or `null` for a group with no views this session may
 * reach. In order: the key the user is on, where the current view's group shares keys with this
 * one, so a layout toggle keeps the key; else `lastLeft`, the key the caller last left this group
 * on; else the group's first view in creation order.
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
