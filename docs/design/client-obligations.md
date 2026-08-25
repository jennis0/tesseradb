# Client obligations

**Status:** Provisional — the twelve rules of `client-components.md` §3, each verified against the
code it points at (2026-08-25). Becomes normative on the owner's call; until then a conflict with
`contracts.md` or `client-interaction.md` is resolved in their favour.

**Owns:** the rules the server cannot enforce because they are about presentation, and what goes
wrong on the screen when each is broken. The server enforces everything about *what* a principal
is shown — every count, sample and artifact is computed from inside their own mask. It sees one
request at a time and cannot see a screen, so the rules below are the client's, and a client that
breaks one can turn a fail-closed answer into a misleading display without a byte leaking.

The API is described in [`../openapi/tessera.yaml`](../openapi/tessera.yaml) and the framing is
walked in [`../openapi/README.md`](../openapi/README.md); `contracts.md` §3 and §5 are the
contract. The built client (`clients/ts/core`) is the reference for each rule and is named where
it keeps one.

## The twelve

1. **The display states are distinct, and only `shown` carries a number.** A view is `idle`,
   `loading`, `retrying`, `shown`, `empty` or `refused` — and `shown` may additionally be *stale*
   (rule 3). `empty` is a real answer of zero; `refused` is *unknown*; `loading` is *not yet*. Only
   `shown` has a count to display, and a number rendered under any other state is a number of
   nothing. *Broken:* a failed request drawn as an empty map converts fail-closed into
   fail-misleading — the server refused to answer and the screen says "nothing here"
   (client-interaction §9). The two are semantic opposites and may never be collapsed.

2. **Both figures of a sample, or neither.** Anything that is a served *sample* of a set — the
   marks drawn, `served`, the items inside a selection — is shown as *shown of total* (`48 of
   1,366`), and shown as nothing at all when the total is not exact or the view is stale. Never
   a bare sample count. *Broken:* the sample masquerades as the set, and a reader takes 48 to be
   the population — the confidently wrong downstream analysis client-interaction §4's P2 exists to
   prevent. The built client types every such value as `Count = {shown, total, exact}` so a
   custom panel cannot render one figure without deliberately destructuring it.

3. **A stale view is marked, and refresh is reachable.** Staleness is accepted here on the
   condition that it is visible (client-interaction §4, owner ruling 2026-07-31): a view drawn
   under one content key while a later response carried another is *shown-but-stale*, and says
   so, and the control that refreshes it is one gesture away. **The signal is the content key** —
   the `etag` on every viewport response (`delta-serving.md` §2; `replica.ts` exposes it as
   `currentContentKey`) — **not `x-tessera-stale`**, which is the broadcast geometry stamp and
   moves at every flush, merge and compaction whether or not anything this principal can see
   changed. *Broken one way:* an unmarked stale display is the truthfulness failure the staleness
   concession quietly buys. *Broken the other way:* a client wired to `x-tessera-stale` declares
   itself stale every tick under continuous ingest and the mark means nothing.

4. **A masked count is a count of what *you* can see, and is never a size.** `visible`,
   `matched`, an artifact's `masked_count`, a region's counts: each is the asking principal's own
   figure, computed inside their mask. The wire never carries a membership size, an ordinal or an
   unmasked quantity (contracts §3.2, the artifacts frame), so nothing a client holds can be
   labelled *of N* against a cluster. *Broken:* "12,040 of 12,040" beside a cluster is false, not
   secret — it asserts a total the client was never given. The built client types these as
   `Masked = {value, exact}`: one figure or none, and no denominator to reach for.

5. **An absent artifact has no reason, and an absent value is an empty answer.** The artifacts
   frame is absent — never empty — when nothing is served, and a client reads that as *nothing
   here* and never as *no layer*, *out of view* or *below threshold*, because the server made
   those one outcome on purpose (contracts §3.2). The same shape on the other surface: a filter
   naming a value that does not exist and one naming a value this principal may not see both
   return no matches, and a client renders the two identically — the one filter rule the server
   states for clients. *Broken:* a UI that says "no such value" on an empty answer has turned the
   filter into an existence oracle over exactly the vocabulary `visibility = "derived"` hides;
   one that says "layer not reachable" has done the same for layers.

6. **The artifact channel asks for itself.** Artifacts are requested by a request that names the
   layers on (`layers: [...]`) and, when only the artifacts are wanted, sends `k = 0` — the
   counts-only idiom contracts §3.2 states. They are never read off the point path of a replica.
   *Broken:* the replica elides tiles it already holds and an elided tile contributes no
   artifacts, so a cluster's presence would depend on whether its ground happened to be novel and
   the map would **lose clusters as the cache warmed** — the worst kind of bug, because the cache
   working is what makes it appear (`clients/ts/core/src/artifactChannel.ts` states this at the
   site). Point requests send `layers: []` for the same reason; with the membership column (D12)
   they name the layers on and pay the pass.

7. **Held state drops when the identity key or the filter changes.** The replica, held
   artifacts, the selection, per-column value lists and the encoding accumulators go on an
   identity-key change — a different principal's picture must not sit under the new one's. Held
   bands also go on a filter change, because the identity key deliberately excludes filters and
   the server cannot tell a client its holdings no longer match (`delta-serving.md` §2). A held
   whole-layer artifact set goes when the content key it was fetched under rotates, and on
   refresh. *Broken:* marks or clusters a previous principal was served stay on screen under a
   new token, or a filtered view keeps drawing items the filter excluded.

8. **`k` never decreases on zoom.** Design §7.2's nesting — a mark drawn in a parent tile is
   still drawn in the child containing it — holds for a fixed cap, and the server sees one request
   at a time and cannot enforce it (contracts §3.2). *Broken:* marks pop out as the user zooms
   in, which reads as items disappearing under their eyes.

9. **A `tessera_id` is a `u64`.** A decimal string in JSON (the control plane's publish
   responses, the path segment of `/v1/items/{tessera_id}` and `/v1/artifacts/{tessera_id}`), a
   `BigInt` off Arrow, and **never a JS `number`**, which loses bits past 2⁵³ — the golden
   fixture's first id already does. *Broken:* a pick returns `404 unknown` for an item that is on
   screen, or the record of a different item, and nothing says why. The built client's `ids` is a
   `BigUint64Array`.

10. **A same-origin proxy in front of the viewer plane forwards six headers.** A replica is keyed
    and revalidated by them, and each is set unconditionally by `/v1/viewport`
    (`crates/tessera-server/src/viewer.rs`): `etag` (the content key — what a held band may be
    *declared* under), `x-tessera-identity-key` (the cache partition key — what a held band may be
    *rendered* under at all), `x-tessera-pin` (the stamp a request echoes back), `x-tessera-stale`
    (the broadcast geometry signal), `x-tessera-server-us` and `x-tessera-admission-us` (the two
    timings). A proxy that strips headers it does not know keeps none of them. *Broken:* without
    the identity key the replica cannot partition by principal and one principal's cache can sit
    under another's token; without the etag it can never declare what it holds and pays the full
    response on every request; without the pin it cannot echo the stamp and the stale bit is
    meaningless. (The obligation is the proxy's because until D10 lands there is no viewer-plane
    CORS for production, so a browser reaches the viewer plane only through one.)

11. **A `401` on a token that previously worked is the session ending, the same as a `403`.** The
    split is best-effort (contracts §3.1): the server answers `401 bad-credential` for a token it
    does not hold — including one it held until it was revoked — and `403 expired-token` for one
    it holds whose `expires_at` has passed (`crates/tessera-server/src/state.rs`,
    `authenticated_session`). Either way the session is over and the client re-authorises.
    *Broken:* a client that reads `401` as *fix your credentials* stalls on a revoked session, or
    retries a request that will never succeed.

12. **Depth is the client's choice and is not on the wire.** The request carries a `zoom` and a
    tile set; the *number of marks on screen* is `m_target × tiles-in-view`, so the only lever is
    which depth's tiles are asked for, and no server field derives it. The built formula
    (`clients/ts/core/src/budget.ts`, `chooseDepth`), stated so that a client written from the
    description can reproduce it:

    - `wantedTiles = max(1, budget / m_target)`, where `budget` is the client's own drawn-mark
      budget and `m_target` starts at `/v1/meta`'s `selection.theta_target_marks`;
    - walk `d` from **3** upward (shallower cannot deliver — at depth 0 a view holds one tile and
      about sixteen marks whatever the budget says) to 16, stopping at the first depth whose
      tile count over the view reaches `wantedTiles` (*budget*), stepping back one when the next
      depth's tile count would exceed `selection.max_tiles_per_request` (*cap*), and stopping
      early once `tiles × m_target ≥ visible` for the previous response's `visible` over the view
      (*saturated* — nothing deeper can add marks);
    - after each response, correct `m_target` toward what was served: `ratio = served /
      predicted`, damped by half, clamped to `[0.25, 4] × theta_target_marks`, skipped when
      saturated, and applied only across motion so a still view never re-derives and nothing
      pops.

    *Broken without the floor:* seventeen marks at full extent. *Broken without the saturation
    term:* a sparse principal's depth ratchets to the 262,144-tile cap to deliver 1,366 marks,
    because the unsaturated model predicts millions (measured at 10⁹). *Broken without the
    damping and the motion rule:* overshoot on dense ground reached 4–8× the budget, and marks
    popped out of a still view when the correction landed.

## What this page does not say

Nothing here is a disclosure rule. Every count, sample and artifact a client receives has already
been computed inside the principal's mask; these are the rules for not *misrepresenting* what
was received. The leak register is `architecture.md` Appendix C, and a client cannot add to it —
what it can do is draw a true answer falsely, which is what each *broken* line above describes.
