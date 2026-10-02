/**
 * What the operator scripts share: reading a control answer, minting a session, the principals the
 * demo's viewer reads as, and the two layer declarations they send. Core's live test sends the same declarations to a real server, so a declaration the server
 * refuses fails there. This file imports nothing, so Node loads it by stripping its types.
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

/** The catalogue verbs of `Control` the helpers below call. */
type Catalogue = {
  createPrincipal(name: string, kind: 'person' | 'service'): Promise<Answered>;
  grant(grant: {principal: string; terms?: string[]; permission?: 'read' | 'authorise-as'}): Promise<Answered>;
  createKey(principal: string): Promise<Answered>;
};

/** Creates a principal, or finds the one a previous run created under the name. */
async function ensurePrincipal(control: Catalogue, name: string, kind: 'person' | 'service'): Promise<void> {
  const created = await control.createPrincipal(name, kind);
  if (created.status !== 409) accepted(`create principal ${name}`, created);
}

/**
 * The name of a local principal holding `read` and `terms`, named by a digest of the terms, so a
 * later run finds the principal an earlier one made. `public` is held by every session and granted
 * to none, so it is left out.
 */
export async function principalHolding(control: Catalogue, terms: string[]): Promise<string> {
  const held = [...new Set(terms.map((t) => t.trim()))].filter((t) => t && t.toLowerCase() !== 'public').sort();
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(JSON.stringify(held))));
  const name = `holding-${Array.from(digest.subarray(0, 8), (b) => b.toString(16).padStart(2, '0')).join('')}`;
  await ensurePrincipal(control, name, 'person');
  accepted(`grant read to ${name}`, await control.grant({principal: name, permission: 'read'}));
  accepted(`grant ${held.length} terms to ${name}`, await control.grant({principal: name, terms: held}));
  return name;
}

/** A new API key of the service principal `name`, which is created holding `authorise-as` where it is absent. */
export async function integratorKey(control: Catalogue, name: string): Promise<string> {
  await ensurePrincipal(control, name, 'service');
  accepted(`grant authorise-as to ${name}`, await control.grant({principal: name, permission: 'authorise-as'}));
  return String(accepted(`create a key for ${name}`, await control.createKey(name)).key);
}

/**
 * `POST /session/authorise`: a session for `terms`, with the operator credential, or for a local
 * `principal`, with an API key holding `authorise-as`.
 */
export async function authorise(
  sessionUrl: string,
  credential: string,
  target: {terms: string[]} | {principal: string}
): Promise<{token: string; token_id: number; expires_at: number}> {
  const r = await fetch(`${sessionUrl}/session/authorise`, {
    method: 'POST',
    headers: {authorization: `Bearer ${credential}`, 'content-type': 'application/json'},
    body: JSON.stringify(target)
  });
  if (!r.ok) throw new Error(`authorise: ${r.status} ${await r.text()}`);
  return r.json();
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
