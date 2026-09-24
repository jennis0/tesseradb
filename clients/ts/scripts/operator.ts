/**
 * What the operator scripts share: reading a control answer, and the two layer declarations they
 * send. Core's live test sends the same declarations to a real server, so a declaration the server
 * no longer takes fails there. This file imports nothing, so Node loads it by stripping its types.
 */

/** A control answer, as `Control` returns it. */
type Answered = {ok: boolean; status: number; text: string; body: Record<string, unknown>};

/** A layer declaration in the wire's own names. */
export type LayerDeclaration = {name: string; [field: string]: unknown};

/** A control answer's body, or a throw naming the refusal. */
export function accepted(what: string, answer: Answered): Record<string, any> {
  if (!answer.ok) throw new Error(`${what}: ${answer.status} ${answer.text}`);
  return answer.body;
}

/**
 * A flat layer of clusters on one view, each artifact inheriting the layer's own label. The
 * computed properties are recomputed per viewer from the members that viewer sees.
 */
export function clusterLayerDeclaration(options: {
  name: string;
  title: string;
  view: string;
  /** The label a viewer must hold to know the layer exists; `null` is public. */
  visibility: string | null;
  /** How many of an artifact's members a viewer must see for it to exist; `null` sets no floor. */
  minVisible: number | null;
  computed: string[];
}): LayerDeclaration {
  return {
    name: options.name,
    title: options.title,
    views: [options.view],
    membership: 'enumerated',
    visibility: options.visibility,
    artifact_visibility: {field: null, default: 'inherited'},
    require_member_visibility: options.minVisible === null ? null : {count: options.minVisible},
    hierarchy: {kind: 'flat', prune_children: false},
    content: {computed: options.computed, supplied: []},
    depends_on: [],
    levels: []
  };
}

/**
 * A flat layer of text labels attached to the artifacts of `clusters`. The text is served only to
 * a viewer who sees every member it was generated from, and a deleted member withdraws it at the
 * fold. `depends_on` names the cluster layer, since an attachment into an undeclared layer is
 * refused.
 */
export function labelLayerDeclaration(options: {name: string; title: string; view: string; clusters: string}): LayerDeclaration {
  return {
    name: options.name,
    title: options.title,
    views: [options.view],
    membership: 'enumerated',
    visibility: null,
    artifact_visibility: {field: null, default: 'inherited'},
    require_member_visibility: null,
    hierarchy: {kind: 'flat', prune_children: false},
    content: {computed: [], supplied: [{name: 'label', type: 'text', require_member_visibility: 'all'}]},
    depends_on: [options.clusters],
    levels: []
  };
}
