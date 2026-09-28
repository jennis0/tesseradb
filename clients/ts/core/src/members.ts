import type {ClauseVerb} from './filters.js';
import type {FilterExpr, MemberOfOperand} from './types.js';

/**
 * The `member_of` leaf and the clauses a client holds of it.
 *
 * A `member_of` clause names one artifact of one layer and asks for its members. It composes like
 * any other leaf and sits in `filters` or in `highlight` alike, so narrowing the map to a cluster
 * and lighting that cluster's members are one clause in two positions, and an artifact can hold a
 * clause in each at once. A drawn `region` asks about a shape and
 * this asks about membership; for an artifact whose members are spread across the map, its shape
 * is the map's outline and the two differ.
 */

/**
 * The `member_of` operand for an artifact held as a `bigint`: the layer name, and the artifact's
 * `tessera_id` as a decimal string.
 *
 * @category Filters
 */
export function memberOf(layer: string, artifact: bigint): MemberOfOperand {
  return {layer, artifact: artifact.toString()};
}

/**
 * One `member_of` clause as the interface holds it: an artifact, whether the clause selects its
 * members or everything outside them, and which expression it joins. {@link withMember} keeps one
 * clause per artifact in each position, so an artifact can be filtered to and highlighted at once.
 *
 * @category Filters
 */
export type MemberClause = {
  /** The artifact's layer. */
  layer: string;
  /** The artifact's `tessera_id`. */
  artifact: bigint;
  /** Whether the clause selects everything outside the artifact (`none_of` over the leaf) in place of its members. */
  outside: boolean;
  /** The expression the clause joins. */
  verb: ClauseVerb;
  /**
   * What the interface called the artifact when the clause was made. It is for display and is not
   * sent. A clause on a filter layer names an artifact the viewport does not serve, so only the
   * panel or card that made the clause knew a name. Absent where the caller had none.
   */
  label?: string;
};

/**
 * A string naming one artifact of one layer, `<layer> <artifact>`, for keying a `Map` or `Set`.
 * Two clauses on one artifact have the same key, whatever their positions.
 *
 * @category Filters
 */
export function memberKey(layer: string, artifact: bigint): string {
  return `${layer} ${artifact}`;
}

/**
 * One clause as a filter leaf: `member_of`, inside `none_of` where the clause is `outside`.
 *
 * @category Filters
 */
export function memberLeaf(clause: MemberClause): FilterExpr {
  const leaf: FilterExpr = {member_of: memberOf(clause.layer, clause.artifact)};
  return clause.outside ? {none_of: [leaf]} : leaf;
}

/**
 * Joins the clauses in position `verb` to `expr` with `all_of`. Two artifacts named together narrow
 * to their intersection; to show the members of either, name their parent. Returns `expr` where no
 * clause is in that position, and the bare leaf where `expr` is `null` and one clause is.
 *
 * @category Filters
 */
export function withMembers(expr: FilterExpr | null, clauses: readonly MemberClause[], verb: ClauseVerb): FilterExpr | null {
  const leaves = clauses.filter((c) => c.verb === verb).map(memberLeaf);
  if (leaves.length === 0) return expr;
  const all = expr === null ? leaves : [expr, ...leaves];
  return all.length === 1 ? all[0]! : {all_of: all};
}

/**
 * Returns `clauses` with `clause` added last, in place of any clause that names the same artifact
 * in the same position. A clause on the artifact in the other position is kept.
 *
 * @category Filters
 */
export function withMember(clauses: readonly MemberClause[], clause: MemberClause): MemberClause[] {
  return [...withoutMember(clauses, clause.layer, clause.artifact, clause.verb), clause];
}

/**
 * Returns `clauses` without the clause naming this artifact in position `verb`, if there is one.
 *
 * @category Filters
 */
export function withoutMember(clauses: readonly MemberClause[], layer: string, artifact: bigint, verb: ClauseVerb): MemberClause[] {
  const key = memberKey(layer, artifact);
  return clauses.filter((c) => c.verb !== verb || memberKey(c.layer, c.artifact) !== key);
}
