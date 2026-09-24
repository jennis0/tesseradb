import type {ClauseVerb} from './filters.js';
import type {FilterExpr, MemberOfOperand} from './types.js';

/**
 * The `member_of` leaf and the clauses a client holds of it.
 *
 * A `member_of` clause names one artifact of one layer and asks for its members. It composes like
 * any other leaf and sits in `filters` or in `highlight` alike, so narrowing to a cluster and
 * lighting a descriptor are one clause in two positions. A drawn `region` asks about a shape and
 * this asks about membership; for an artifact whose members are spread across the map, its shape
 * is the map's outline and the two differ.
 */

/** The leaf's operand for an artifact a client holds as a `bigint`. */
export function memberOf(layer: string, artifact: bigint): MemberOfOperand {
  return {layer, artifact: artifact.toString()};
}

/**
 * One clause the interface holds: an artifact, whether it means this or outside this, and which
 * expression it joins. An artifact is in one clause at most, so filtering to an artifact already
 * highlighted moves its clause.
 */
export type MemberClause = {
  layer: string;
  artifact: bigint;
  /** `none_of` over the leaf: outside this artifact. */
  outside: boolean;
  verb: ClauseVerb;
  /**
   * What the interface called the artifact when the clause was made, for display only and not sent.
   * A clause on a filter layer names an artifact the viewport does not serve, so only the panel or
   * card that made it knew a name. Absent where the caller had none.
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
 * The clauses in one position, joined to `expr` by `all_of`: two nodes named together narrow to
 * their intersection. A viewer who wants either names their parent.
 */
export function withMembers(expr: FilterExpr | null, clauses: readonly MemberClause[], verb: ClauseVerb): FilterExpr | null {
  const leaves = clauses.filter((c) => c.verb === verb).map(memberLeaf);
  if (leaves.length === 0) return expr;
  const all = expr === null ? leaves : [expr, ...leaves];
  return all.length === 1 ? all[0]! : {all_of: all};
}

/** Adds a clause, or moves the one already naming this artifact. */
export function withMember(clauses: readonly MemberClause[], clause: MemberClause): MemberClause[] {
  const key = memberKey(clause.layer, clause.artifact);
  return [...clauses.filter((c) => memberKey(c.layer, c.artifact) !== key), clause];
}

/** Drops the clause naming this artifact, if there is one. */
export function withoutMember(clauses: readonly MemberClause[], layer: string, artifact: bigint): MemberClause[] {
  const key = memberKey(layer, artifact);
  return clauses.filter((c) => memberKey(c.layer, c.artifact) !== key);
}
