# Client components — delivery and status

**Status:** Living. **This file is the status record for the client-components work** — the steps,
what each must be true to be finished, and where it has got to. Tracked here rather than in
GitHub issues, on the convention the artifact work set ([`artifact-delivery.md`](artifact-delivery.md)).

**The rule that keeps it true: it moves in the change that moves the work.** A step that landed
without this file changing is a step whose status is now wrong, and a file that disagrees with the
code is the file that is wrong.

**How the work is done:** one worktree per step, branched from the previous step's merge, the gate
green before merge, this file updated in the same change. [`client-handover.md`](client-handover.md)
is the map — the work list expanded, the traps, the parallel tracks; the design is
[`design/client-components.md`](design/client-components.md) (r4), whose §9 is normative for the
order and §11 for the decisions.

## Where it stands

Nothing is built. The design is at r4, reviewed across three lenses, with the owner's rulings of
2026-08-24/25 recorded in its §11 and awaiting decision files; the open decisions and what each
gates are in the handover's §1. The instrument in `clients/ts/` — `@tessera/client` and
`@tessera/viewer` — is the starting point for step 0.

## The steps

| step | what lands | proved by | needs | status |
|---|---|---|---|---|
| 0 | the presented frame into the client: the store holds the `Composition`, absorbs `binding.ts`'s writes; the viewer's duplicate `ViewState` goes | smoke green; `check-clients.sh` | — | not started |
| 1 | the store: `createStore`, the projections, `Count`/`Masked`, `stale` on the content key, `setView`'s conversion, `dataXY`, `extentOf`, the encoding accumulators, the artifact channel and the panel fetches out of the viewer, the session artifact table; the `@tesseradb` rename | smoke green; store tests | D3 | not started |
| 2 | `@tesseradb/deck` and `@tesseradb/components`: `TesseraLayer`, the slab, the density texture; nine elements incl. `<tessera-explorer>`; box selection; the eight states; the §5.9 mechanics; the viewer as the explorer plus instruments | smoke through shadow-piercing locators; the harness's first assertions | D1, D2, D2a, D5, D7 | not started |
| 3 | layer picker with closure, artifact list and card, legend, wire geometry drawn, sidecar retired, lasso; **with D12:** the membership attribute, the lookup texture, colour coverage | `smoke-artifacts.mjs` under two principals; the harness | D12 for colour | not started |
| 4 | the examples (plain HTML, React explorer, canvas store) and `@tesseradb/react`, in the gate | typecheck; the harness against the C1 page | D10 for the production paragraph | not started |
| 5 | `tesseradb[widget]`: `Map`, operator-only `authorise`, the messages, the traitlets, the wheel's build hook | the notebook example; the build hook in the gate | D4, D10 | not started |
| 6 | C3's documents: contracts §3.2 amended, the OpenAPI description, worked decodes with a test, the obligations list | the decode test; `check-doc-links.py` | D9 | not started |

**Server tracks the design asks for** (each its own design and worktree; status kept here so the
client steps can see what they wait on):

| track | ask | gates | status |
|---|---|---|---|
| S1 | D10 — viewer-plane `serve.cors_origins` | step 4's production docs, step 5 | not started |
| S2 | D9 — the wire idioms at the contracts amendment | step 6 | **`layers` built on `server/s3-membership`** (2026-08-25): omitted or `[]` is none, `"all"` is every reachable layer, `all` refused as a name; the contracts wording is in S3's report for step 6 to fold |
| S3 | D12 — the per-point membership column, deepest served, with its leak-register pass | step 3's colouring | **built** on `server/s3-membership` (2026-08-25): `membership:<layer>` per served layer, nullable `u64`, after the scalars; both layouts; measured; the register row (C30) is **proposed** in [the memo](evidence/memos/2026-08-25-d12-membership-column.md), **owner ruling pending** |
| S4 | D13 — a dependent artifact's target | label counts | **ruled and built** on `server/s3-membership` (2026-08-25): not the target's id — a dependent carries its target's masked count; register note proposed in S3's memo; the drill-down route left for a ruling |
| S5 | D8 — the fetch-model hint in `/v1/meta` | nothing; the store observes | not started |
| S6 | D11 — the selection operand; the export verb; the runtime-artifact path | *filter to this*, *export*, *save* | not started |
| S7 | D14 — a verified-assertion auth plugin | C1's production token story | not started |

## What each step owes a measurement

The design's §5.10 figures are modelled. Step 2's harness measures, and this file records:
per-response decode and remap cost at the render target; per-settle resolve, coverage check,
texture writes and label placement; frame time at several million marks with the lookup texture;
the layer-switch refill time under the colour-stale refetch. A figure quoted anywhere else is
re-measured before it is trusted.
