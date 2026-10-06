# The wire, for a stranger

Three things a power user who installs nothing needs, and where each is:

| | where | kept true by |
|---|---|---|
| The API | [`tessera.yaml`](tessera.yaml), OpenAPI 3.1 for the viewer, session and control planes | `crates/tessera-server/tests/openapi.rs`, which starts the server, exercises every route with a success and a refusal, and validates every JSON body against the description's schemas |
| The framing, to the byte | [The framing](#the-framing) and [the worked decodes](#the-worked-decodes) below | the viewport decodes' tests, over the golden fixtures in `clients/ts/core/test/fixtures/`; the items decodes have none |
| What the server cannot enforce | [The twelve rules](../system/clients.md#the-twelve-rules) in the clients chapter | reading it |

## What the description is, and is not

`tessera.yaml` is **hand-authored**. The JSON DTOs live in `tessera-server`, some private, the
control plane's answers built with `json!`, and the Arrow-facing structs carry a deliberate *no serde derive*. Nothing generates the description from them, so the test is what stops it drifting:
every closed DTO is declared `additionalProperties: false`, and a field added to a response and
not to the file fails the test rather than surfacing on a stranger's screen.

What it can say only in prose: the bodies of `/v1/viewport`, `/v1/artifacts/viewport`,
`/v1/items`, `/v1/artifacts` and `/v1/aggregate` are not JSON, and are declared as
`application/octet-stream` with the framing described beside them; and a `membership` or
`arrow_type` value is engine-derived, so the description names its type and says the set is not
enumerated there.

`layers` omitted or `[]` means no layers, the string `"all"` means every layer the principal
reaches, and an array means the named layers the principal reaches. On `/v1/viewport` the layers
named tag the points; on `/v1/artifacts/viewport` they are the layers whose artifacts are served.

## The framing

A `POST /v1/viewport` body is a sequence of frames, each `u8 kind`, `u32` little-endian payload
length, payload. Every Arrow payload is a **complete IPC stream**: `pyarrow.ipc.open_stream`,
Arrow JS's `tableFromIPC` and arrow-rs's `StreamReader` each consume one whole. The trailer is
JSON. A reader dispatches on `kind` without parsing any Arrow metadata.

```
kind 1  tiles      exactly one, first        (tile: u64, visible: u64, matched: u64, served: u64, highlighted: u64)
kind 2  sub-cells  exactly one iff requested (cell: u64, count: u64)
kind 3  points     zero or more, whole tiles per frame; the frames concatenate
kind 4  trailer    exactly one, last; JSON with exactly {stream_us, arrow_serialise_ns, points, flushes}
```

A decoder keeps two rules. **The trailer is the completeness signal**: a body without a trailing
kind-4 frame is incomplete whatever the transport said, though every prefix is sound to draw,
since the counts are exact from the first frame. **An unknown kind is an error**, never skipped.

The points batch is `(tessera_id: u64, code: u64, …render columns)`, the render columns in
`/v1/meta`'s `declared_scalars` order, each named by its column. A render column is nullable: a
point whose item has no value in it is null, and a zero is a value. A category is the exception:
it has no nulls, and its code 0 means no value. `code` is the position: 32 bits
per axis, Morton-interleaved; deinterleave and scale against `/v1/meta`'s `quantisation` to
recover coordinates. A request with a `highlight` adds a `highlighted` bool after the render
columns. Each layer the request names that serves an artifact holding a served point then adds a
`membership:<layer>` column of `uint64`: the `tessera_id` of the deepest artifact of that layer the
point belongs to and the principal is served, or null. `served` on the tiles batch is how many
points each tile contributed, in order, which is how a reader splits the flat concatenation back
into tiles.

A `POST /v1/artifacts/viewport` body has the same framing, with kind-5 artifacts frames: one for
the nested and `dag` layers first, where it holds a row, and then exactly one for each tile, in the
request's order with repeats removed. Each has seventeen columns ending in `tile`, which is null in
the first. The trailer is exactly `{stream_us, arrow_serialise_ns, rows, frames}`. **A frame of no
rows carries no reason**: no layer reached, none in the tile and none clearing its criterion are
one outcome by design.

## The worked decodes

Two runnable examples, each importing nothing of Tessera's, each printing the frames of a raw body
and the first rows of every batch, and each tested over the same fixtures to the same answer
(`clients/ts/wire-example/test/expected.json` is the shared answer sheet).

**Python, with `pyarrow`**: `reference/examples/decode_viewport.py`, here over a viewport body
whose request named a layer:

```
$ python3 reference/examples/decode_viewport.py clients/ts/core/test/fixtures/viewport-membership.bin
clients/ts/core/test/fixtures/viewport-membership.bin: 12068 bytes, 3 frames
  kind 1 tiles: 1736 B, 15 rows, columns ['tile', 'visible', 'matched', 'served', 'highlighted']
  kind 3 points: 10248 B, 255 rows, columns ['tessera_id', 'code', 'archive', 'primary_category', 'submitted_at', 'membership:clusters/kmeans']
  kind 4 trailer: 69 B  {"arrow_serialise_ns":26507,"flushes":1,"points":255,"stream_us":758}
first tiles rows: [{'tile': 0, 'visible': 86, 'matched': 86, 'served': 2, 'highlighted': 86}, {'tile': 1, 'visible': 2985, 'matched': 2985, 'served': 27, 'highlighted': 2985}, {'tile': 4, 'visible': 649, 'matched': 649, 'served': 7, 'highlighted': 649}]
...
```

The whole decoder is `split_frames` (the framing, no Arrow) and `decode_viewport` (one
`ipc.open_stream(...).read_all()` per Arrow payload, `json.loads` for the trailer). Its test is
`reference/tests/test_wire_example.py`; run `python3 -m pytest reference/tests/test_wire_example.py`.
It skips, saying so, if `pyarrow` is not importable.

**JavaScript, with `apache-arrow`**: `clients/ts/wire-example/src/decode-viewport.mjs`, its own
workspace under `clients/ts` with no dependency on the client packages, here over a
`POST /v1/artifacts/viewport` body:

```
$ cd clients/ts/wire-example && node src/decode-viewport.mjs ../core/test/fixtures/viewport-artifacts.bin
../core/test/fixtures/viewport-artifacts.bin: 81754 bytes, 17 frames
  kind 5 artifacts: 4936 B, 2 rows, columns [layer, tessera_id, key, masked_count, centroid_x, centroid_y, box_min_x, box_min_y, box_max_x, box_max_y, content, parent_ids, rung, matched, highlighted, target, tile]
  kind 5 artifacts: 5320 B, 10 rows, columns [layer, tessera_id, key, masked_count, centroid_x, centroid_y, box_min_x, box_min_y, box_max_x, box_max_y, content, parent_ids, rung, matched, highlighted, target, tile]
...
  kind 5 artifacts: 2696 B, 0 rows, columns [layer, tessera_id, key, masked_count, centroid_x, centroid_y, box_min_x, box_min_y, box_max_x, box_max_y, content, parent_ids, rung, matched, highlighted, target, tile]
...
  kind 4 trailer: 69 B  {"arrow_serialise_ns":522516,"frames":16,"rows":114,"stream_us":2120}
first tiles rows: []
first artifacts rows: [
  {
    layer: 'clusters/kmeans',
    tessera_id: '9805232920100346745',
    key: 'km-000014',
    masked_count: '571',
...
```

That fixture answers a request for one layer of clusters at zoom 2 with a `per_tile` of 50. It holds one
frame for each of the sixteen tiles, one of them empty, and no frame for nested or `dag` layers. The `u64` columns come off Arrow JS as `BigInt` and are printed as
decimal strings; a decoder that narrows a `tessera_id` to a JS `number` has already lost bits on
this fixture's first id. Its test is `clients/ts/wire-example/test/decode.test.ts`, run by
`bash scripts/check-clients.sh` with the rest of the client gate.

Both decoders are strict on purpose: a truncated body, a missing trailer and an unknown kind each
raise, and both tests prove it on the fixtures.

## The items read

A `POST /v1/items` body uses the same framing with four kinds:

```
kind 6  head      exactly one, first          JSON {order, page_rows, visible?, matched?}
kind 7  records   one or more                 one Arrow IPC stream holding one batch
kind 8  page end  one after each records frame JSON {next, ended_by}
kind 4  trailer   exactly one, last           JSON {pages, rows, next, ended_by, stream_us}
```

A response that finds no row carries one page of no rows, so every response gives the read's
columns and their types; only a response cancelled before its first page, by the stream deadline
or because the client has gone, carries none.

A reader keeps three rules. A body without a trailer is incomplete, however it was cut, and the
read resumes from the cursor in the last page end received, which both decoders below hand back
with the pages before it. A records frame that no page end follows is discarded. A
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


class IncompleteRead(Exception):
    def __init__(self, cursor, batches):
        super().__init__(f"no trailer; resume from cursor {cursor!r}")
        self.cursor, self.batches = cursor, batches


def frames(body: bytes):
    # A cut frame ends the walk; the caller sees no trailer.
    at = 0
    while len(body) - at >= 5:
        kind, length = struct.unpack_from("<BI", body, at)
        payload = body[at + 5 : at + 5 + length]
        if len(payload) != length:
            return
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
        # Incomplete: resume with the last page end's cursor, dropping any unfinished page.
        raise IncompleteRead(cursor, batches)
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
  // A cut frame ends the walk; the caller sees no trailer.
  for (let at = 0; body.length - at >= 5; ) {
    const kind = body[at];
    const length = view.getUint32(at + 1, true);
    const payload = body.subarray(at + 5, at + 5 + length);
    if (payload.length !== length) return;
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
  if (!trailer) {
    // Incomplete: resume with the last page end's cursor, dropping any unfinished page.
    throw Object.assign(new Error(`no trailer; resume from cursor ${cursor}`), { cursor, tables });
  }
  return { head, tables, trailer };
}

const { head, tables, trailer } = decodeItems(new Uint8Array(await response.arrayBuffer()));
```

`tessera_id` arrives as a `uint64`, which Arrow JS reads as a `BigInt`; narrowing it to a `number`
loses bits. Both decoders were run against zstd-compressed and uncompressed bodies from a release
server, and without the registered codec Arrow JS refuses the compressed one. Neither has a test of
its own, as the viewport decodes do.

## The aggregate read

A `POST /v1/aggregate` body uses the same framing, with no head of its own and one table per
grouping, in the order of the request's `groupings`:

```
kind 9  table head before a table's first page JSON {grouping, total, reference_total?, groups?, resumed}
kind 7  records    one or more per table        one Arrow IPC stream holding one batch of its rows
kind 8  page end   one after each records frame JSON {next, ended_by}
kind 4  trailer    exactly one, last            JSON {pages, rows, next, ended_by, recomposed?, stream_us}
```

`recomposed` is present, as `true`, only where a page counted a different state of the corpus from
the page before it. A response can end part-way through a table. The next response, sent with the
trailer's `next` as `cursor`, opens with that table's head again, with `resumed` true. The rules of
the items read hold: no trailer means incomplete, and a whole result passes `next` back until it is
null. A table's rows are the batches between its head and the next head or the trailer, joined
across responses. A field's `key` and `title` are dictionaries whose values differ from page to
page.

**Python, with `pyarrow`**, reusing `frames` from the items decode:

```python
TABLE_HEAD = 9


def decode_aggregate(body: bytes):
    tables, trailer, pending, cursor = [], None, None, None
    for kind, payload in frames(body):
        if kind == TABLE_HEAD:
            tables.append((json.loads(payload), []))
        elif kind == RECORDS:
            pending = ipc.open_stream(payload).read_next_batch()
        elif kind == PAGE_END:
            tables[-1][1].append(pending)
            cursor = json.loads(payload)["next"]
        elif kind == TRAILER:
            trailer = json.loads(payload)
        else:
            raise ValueError(f"unknown frame kind {kind}")
    if trailer is None:
        raise IncompleteRead(cursor, tables)
    return tables, trailer


tables, trailer = decode_aggregate(body)
for head, batches in tables:
    # A dictionary column's values differ between pages, so unify them before joining.
    table = pa.Table.from_batches(batches).unify_dictionaries()
    print(head["grouping"], head["total"], table.to_pylist()[:3])
```
