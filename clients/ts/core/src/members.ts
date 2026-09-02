import type {ClauseVerb} from './filters.js';
import type {FilterExpr, MemberOfOperand} from './types.js';

/**
 * The `member_of` leaf and the clauses a client holds of it (`highlight-and-hierarchy.md` §3, §5).
 *
 * A `member_of` clause names one artifact of one layer and asks for its membership. It composes
 * exactly like every other leaf, and it sits in `filters` or in `highlight` identically — which is
 * the whole of why *narrow to this cluster* and *light this descriptor* are one mechanism: the
 * clause is the same object in two positions, and moving it costs a field.
 *
 * **This replaces the `region`-by-published-artifact spelling** for *filter to this artifact* and
 * *outside this artifact* on the card (§5.5). The drawn-region spelling stays, for a region drawn
 * by hand: `region` asks about a shape and this asks about a membership, and on a layer whose
 * artifacts are spread across the map the two are not the same question — which is the whole
 * reason the leaf exists, a spread artifact's shape being the map's own outline.
 */

/** The leaf's operand for an artifact a client holds as a `bigint`. */
export function memberOf(layer: string, artifact: bigint): MemberOfOperand {
  return {layer, artifact: artifact.toString()};
}

/**
 * One clause the interface holds: an artifact, whether it is *this* or *outside this*, and which
 * of the request's two expressions it joins.
 *
 * Keyed by `(layer, artifact)`: an artifact is in a clause once, in one position, with one sense.
 * Asking to filter to an artifact already highlighted moves it rather than adding a second clause,
 * which is §5.2's *without being re-entered* for a panel node and a card alike.
 */
export type MemberClause = {
  layer: string;
  artifact: bigint;
  /** `none_of` over the leaf — *outside this artifact*. */
  outside: boolean;
  verb: ClauseVerb;
  /**
   * What the interface called the artifact when the clause was made — **presentation only, and
   * never sent**.
   *
   * A clause on a filter layer names an artifact the viewport never serves (§5.4), so nothing on
   * the map can resolve its identifier to a name; the panel or the card that made the clause is
   * the only thing that knew one, and a chip reading `mesh/descriptors 546790` says nothing about
   * what was asked. Absent where the caller had no name to give.
   */
  label?: string;
};

export function memberKey(layer: string, artifact: bigint): string {
  return `${layer} ${artifact}`;
}

/** One clause as its leaf. */
export function memberLeaf(clause: MemberClause): FilterExpr {
  const leaf: FilterExpr = {member_of: memberOf(clause.layer, clause.artifact)};
  return clause.outside ? {none_of: [leaf]} : leaf;
}

/**
 * The clauses in one position, conjoined with `expr`.
 *
 * `all_of`, because several member clauses mean *all of these* the way several filled-in controls
 * do — two descriptors named together narrow to their intersection, which is what a viewer walking
 * a hierarchy and adding a second node means. A viewer who wants either sends one clause naming
 * the parent.
 */
export function withMembers(expr: FilterExpr | null, clauses: readonly MemberClause[], verb: ClauseVerb): FilterExpr | null {
  const leaves = clauses.filter((c) => c.verb === verb).map(memberLeaf);
  if (leaves.length === 0) return expr;
  const all = expr === null ? leaves : [expr, ...leaves];
  return all.length === 1 ? all[0]! : {all_of: all};
}

/** Add a clause, or move the one already naming this artifact. */
export function withMember(clauses: readonly MemberClause[], clause: MemberClause): MemberClause[] {
  const key = memberKey(clause.layer, clause.artifact);
  return [...clauses.filter((c) => memberKey(c.layer, c.artifact) !== key), clause];
}

/** Drop the clause naming this artifact, if there is one. */
export function withoutMember(clauses: readonly MemberClause[], layer: string, artifact: bigint): MemberClause[] {
  const key = memberKey(layer, artifact);
  return clauses.filter((c) => memberKey(c.layer, c.artifact) !== key);
}
