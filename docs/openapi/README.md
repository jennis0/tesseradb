# The wire, for a stranger

Three things a power user who installs nothing needs, and where each is:

| | where | kept true by |
|---|---|---|
| The API | [`tessera.yaml`](tessera.yaml) — OpenAPI 3.1 for the viewer and session planes | `crates/tessera-server/tests/openapi.rs`, which starts the server, exercises every route with a success and a refusal, and validates every JSON body against the description's schemas |
| The framing, to the byte | [`../design/contracts.md`](../design/contracts.md) §5, and the two worked decodes below | the decodes' tests, over the golden fixtures in `clients/ts/core/test/fixtures/` |
| What the server cannot enforce | [`../design/client-obligations.md`](../design/client-obligations.md) | reading it |

The contract is `contracts.md` §3 and §5. Where this directory disagrees with it, this directory
is wrong; the test above is what keeps the disagreement from lasting.

## What the description is, and is not

`tessera.yaml` is **hand-authored**. The JSON DTOs live in `tessera-server`, some private, two
responses built with `json!`, and the Arrow-facing structs carry a deliberate *no serde derive*
(I10 — entity ids never cross the boundary, so nothing about those types is allowed to serialise
itself). Nothing generates the description from them, so the test is what stops it drifting:
every closed DTO is declared `additionalProperties: false`, and a field added to a response and
not to the file fails the test rather than surfacing on a stranger's screen.

What it can say only in prose: `auth_data` is bytes the deployment's auth plugin evaluates, and
the description says exactly that; the `/v1/viewport` body is not JSON and is declared as
`application/octet-stream` with the framing described beside it; and a `membership` or
`arrow_type` value is engine-derived, so the description names its type and says the set is not
enumerated there.

Two request semantics in the file are the owner's rulings of 2026-08-25 and are marked as landing
on the s3 track: `layers` omitted or `[]` means *no* layers, the string `"all"` means every
reachable layer, and an array is intersected with the reachable set; and a dependent artifact's
`masked_count` is its target's. Until that track merges, the server reads an omitted `layers` as
`"all"` — send `[]` for none and an explicit array otherwise, which both servers read identically.
The test that asserts the ruled behaviour is `#[ignore]`d with that reason.

## The framing

A `POST /v1/viewport` body is a sequence of frames, each `u8 kind`, `u32` little-endian payload
length, payload. Every Arrow payload is a **complete IPC stream** — `pyarrow.ipc.open_stream`,
Arrow JS's `tableFromIPC` and arrow-rs's `StreamReader` each consume one whole — and the trailer
is JSON. A reader dispatches on `kind` without parsing any Arrow metadata.

```
kind 1  tiles      exactly one, first        (tile: u64, visible: u64, matched: u64, served: u64)
kind 2  sub-cells  exactly one iff requested (cell: u64, count: u64)
kind 5  artifacts  at most one, before points; absent — never empty — when nothing is served
kind 3  points     zero or more, whole tiles per frame; the frames concatenate
kind 4  trailer    exactly one, last; JSON with exactly {stream_us, arrow_serialise_ns, points, flushes}
```

Three rules a decoder must keep: **the trailer is the completeness signal** — a body without a
trailing kind-4 frame is incomplete whatever the transport said, though every prefix is sound to
draw, since the counts are exact from the first frame; **an unknown kind is an error, never
skipped**; and **absence of the artifacts frame carries no reason** — no layer reached, none
intersecting, none clearing its criterion are one outcome by design.

The points batch is `(tessera_id: u64, code: u64, …render columns)`, the render columns in
`/v1/meta`'s `declared_scalars` order, each named by its column. `code` is the position: 32 bits
per axis, Morton-interleaved; deinterleave and scale against `/v1/meta`'s `quantisation` to
recover coordinates. `served` on the tiles batch is how many points each tile contributed, in
order, which is how a reader splits the flat concatenation back into tiles.

## The worked decodes

Two runnable examples, each importing nothing of Tessera's, each printing the frames of a raw body
and the first rows of every batch, and each tested over the same fixtures to the same answer
(`clients/ts/wire-example/test/expected.json` is the shared answer sheet).

**Python, with `pyarrow`** — `reference/examples/decode_viewport.py`:

```
$ python3 reference/examples/decode_viewport.py clients/ts/core/test/fixtures/viewport-underlay.bin
clients/ts/core/test/fixtures/viewport-underlay.bin: 9840 bytes, 4 frames
  kind 1 tiles: 1160 B, 5 rows, columns ['tile', 'visible', 'matched', 'served']
  kind 2 sub-cells: 776 B, 12 rows, columns ['cell', 'count']
  kind 3 points: 7816 B, 48 rows, columns ['tessera_id', 'code', 'archive', ...]
  kind 4 trailer: 68 B  {"arrow_serialise_ns":50857,"flushes":1,"points":48,"stream_us":319}
first tiles rows: [{'tile': 1, 'visible': 1, 'matched': 1, 'served': 1}, ...]
first sub-cells rows: [{'cell': 30, 'count': 1}, ...]
first points rows: [{'tessera_id': 8549480826753018745, 'code': 2175375494116598736, ...}]
```

The whole decoder is `split_frames` (the framing, no Arrow) and `decode_viewport` (one
`ipc.open_stream(...).read_all()` per Arrow payload, `json.loads` for the trailer). Its test is
`reference/tests/test_wire_example.py`; run `python3 -m pytest reference/tests/test_wire_example.py`.
It skips, saying so, if `pyarrow` is not importable.

**JavaScript, with `apache-arrow`** — `clients/ts/wire-example/src/decode-viewport.mjs`, its own
workspace under `clients/ts` with no dependency on the client packages:

```
$ cd clients/ts/wire-example && node src/decode-viewport.mjs ../core/test/fixtures/viewport-artifacts.bin
../core/test/fixtures/viewport-artifacts.bin: 4140 bytes, 3 frames
  kind 1 tiles: 1416 B, 16 rows, columns [tile, visible, matched, served]
  kind 5 artifacts: 2640 B, 4 rows, columns [layer, tessera_id, key, masked_count, centroid_x, ...]
  kind 4 trailer: 69 B  {"arrow_serialise_ns":551286,"flushes":0,"points":0,"stream_us":9864}
first tiles rows: [ { tile: '0', visible: '640', matched: '640', served: '0' }, ... ]
first artifacts rows: [ { layer: 'clusters/kmeans-v2', tessera_id: '11158655851902647723', key: 'c-0000', masked_count: '2480', ... } ]
```

That fixture is a `k = 0` request naming a layer — the *just the artifacts* idiom: a tiles frame
with `served = 0` everywhere, the artifacts frame, and no points frame. The `u64` columns come off
Arrow JS as `BigInt` and are printed as decimal strings; a decoder that narrows a `tessera_id` to
a JS `number` has already lost bits on this fixture's first id. Its test is
`test/decode.test.ts`, run by `bash scripts/check-clients.sh` with the rest of the client gate.

Both decoders are strict on purpose — a truncated body, a missing trailer and an unknown kind
each raise — and both tests prove it on the fixtures.
