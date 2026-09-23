# The wire, for a stranger

Three things a power user who installs nothing needs, and where each is:

| | where | kept true by |
|---|---|---|
| The API | [`tessera.yaml`](tessera.yaml) — OpenAPI 3.1 for the viewer and session planes | `crates/tessera-server/tests/openapi.rs`, which starts the server, exercises every route with a success and a refusal, and validates every JSON body against the description's schemas |
| The framing, to the byte | [`../design/contracts.md`](../design/contracts.md) §5, and the worked decodes below | the viewport decodes' tests, over the golden fixtures in `clients/ts/core/test/fixtures/`; the items decodes have none |
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

One request semantic in the file is the owner's ruling of 2026-08-25 and is marked as landing
on the s3 track: `layers` omitted or `[]` means *no* layers, the string `"all"` means every
reachable layer, and an array is intersected with the reachable set. Until that track merges, the server reads an omitted `layers` as
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

## The items read

A `POST /v1/items` body uses the same framing with four kinds:

```
kind 6  head      exactly one, first          JSON {order, page_rows, visible?, matched?}
kind 7  records   zero or more                one Arrow IPC stream holding one batch
kind 8  page end  one after each records frame JSON {next, ended_by}
kind 4  trailer   exactly one, last           JSON {pages, rows, next, ended_by, stream_us}
```

A reader keeps three rules. A body without a trailer is incomplete, and the read resumes from the
cursor in the last page end received. A records frame that no page end follows is discarded. A
whole read passes the trailer's `next` back as `cursor` until it is null. With
`compression: "zstd"` the batch's buffers are zstd-compressed inside the Arrow stream, and the
reader needs a zstd codec for them; the framing, the schema and the JSON frames are never
compressed. A category column is a dictionary whose values differ from page to page, since each
page's dictionary holds only the keys its rows carry.

**Python, with `pyarrow`**, which reads zstd-compressed buffers with the codec it ships:

```python
import json
import struct

import pyarrow as pa
import pyarrow.ipc as ipc

HEAD, RECORDS, PAGE_END, TRAILER = 6, 7, 8, 4


def frames(body: bytes):
    at = 0
    while at < len(body):
        kind, length = struct.unpack_from("<BI", body, at)
        payload = body[at + 5 : at + 5 + length]
        if len(payload) != length:
            raise ValueError("truncated body")
        yield kind, payload
        at += 5 + length


def decode_items(body: bytes):
    head, trailer, batches, pending, cursor = None, None, [], None, None
    for kind, payload in frames(body):
        if kind == HEAD:
            head = json.loads(payload)
        elif kind == RECORDS:
            pending = ipc.open_stream(payload).read_next_batch()
        elif kind == PAGE_END:
            batches.append(pending)
            cursor = json.loads(payload)["next"]
        elif kind == TRAILER:
            trailer = json.loads(payload)
        else:
            raise ValueError(f"unknown frame kind {kind}")
    if trailer is None:
        raise ValueError(f"no trailer; resume from {cursor!r}")
    return head, batches, trailer


head, batches, trailer = decode_items(body)
table = pa.Table.from_batches(batches)
```

**JavaScript, with `apache-arrow`**, which reads compressed buffers once a codec is registered for
them. `fzstd` is one such decoder:

```js
import { tableFromIPC, compressionRegistry, CompressionType } from "apache-arrow";
import { decompress } from "fzstd";

compressionRegistry.set(CompressionType.ZSTD, { decode: (data) => decompress(data) });

const HEAD = 6, RECORDS = 7, PAGE_END = 8, TRAILER = 4;
const text = new TextDecoder();

function* frames(body) {
  const view = new DataView(body.buffer, body.byteOffset, body.byteLength);
  for (let at = 0; at < body.length; ) {
    const kind = body[at];
    const length = view.getUint32(at + 1, true);
    const payload = body.subarray(at + 5, at + 5 + length);
    if (payload.length !== length) throw new Error("truncated body");
    yield [kind, payload];
    at += 5 + length;
  }
}

function decodeItems(body) {
  let head, trailer, pending, cursor = null;
  const tables = [];
  for (const [kind, payload] of frames(body)) {
    if (kind === HEAD) head = JSON.parse(text.decode(payload));
    else if (kind === RECORDS) pending = tableFromIPC(payload);
    else if (kind === PAGE_END) {
      tables.push(pending);
      cursor = JSON.parse(text.decode(payload)).next;
    } else if (kind === TRAILER) trailer = JSON.parse(text.decode(payload));
    else throw new Error(`unknown frame kind ${kind}`);
  }
  if (!trailer) throw new Error(`no trailer; resume from ${cursor}`);
  return { head, tables, trailer };
}

const { head, tables, trailer } = decodeItems(new Uint8Array(await response.arrayBuffer()));
```

`tessera_id` arrives as a `uint64`, which Arrow JS reads as a `BigInt`; narrowing it to a `number`
loses bits. Both decoders were run against zstd-compressed and uncompressed bodies from a release
server, and without the registered codec Arrow JS refuses the compressed one. Neither has a test of
its own, as the viewport decodes do.
