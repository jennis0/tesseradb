# Rebuild and replace the bundle

You rebuild the bundle when the sources have changed in ways that are easier to build than to send
to the running server, or when a new version of Tessera can't read the old bundle. The new bundle
goes into a directory of its own while the server keeps running, and replaces the old one in a
short stop.

A rebuild can change the `tessera_id` a browser uses to name an item. The examples use [the
systemd install](systemd.md), and the [Docker guide](docker.md#replace-the-bundle) shows the swap
under Compose.

## What a rebuild keeps

Inside the server every item has an internal number, which never leaves it. A browser gets a
`tessera_id` instead, which is the internal number scrambled with the identity key. The [security
chapter](../system/security.md#a-client-never-sees-an-entity-id) explains why. `tessera build`
needs the key and writes it into the bundle, and the server reads it from there.

The build numbers the items in a fixed order. Items with the same access labels are numbered
together, and within each group the build goes across the map in the order it stores map tiles. So
adding a place, removing one, moving one or changing its labels renumbers the places that come
after it. A rebuild keeps every `tessera_id` only when the declaration and its sources, the key and
the batch size are all unchanged. The batch size matters only if the first build sorted the corpus
in batches. After any other rebuild a `tessera_id` may name a different item, with the key the
same.

The bundle also carries an idset, a number that `/v1/meta` publishes. A client can send it with a
`tessera_id`, and the server refuses the request with a 409 if the number is not the bundle's own.
Raising the idset after a rebuild that renumbered items turns a client's old `tessera_id` into a
refusal, where otherwise it would be answered for whichever item now has that number. A client that
sends no idset gets no such check.

## Build the new bundle

Rebuild with `--carry-id-key-from`, naming the bundle you are serving. It reads the key, the idset
and the batch size from that bundle, so you don't need `.env` for it. Add `--bump-idset` unless the
declaration and its sources are exactly as they were.

The tutorial found Saint Patrick's Bridge in Germany, because GeoNames lists its longitude as
8.47036 where it should be −8.47036. Correct the line in `IE.txt`, convert it again, and copy the
new Parquet file into place:

```console
$ cd ~/ireland
$ awk -F'\t' -v OFS='\t' '$1 == 3302004 { $6 = "-8.47036" } { print }' IE.txt > IE.fixed && mv IE.fixed IE.txt
$ .venv/bin/python convert.py
wrote 29935 places to points.parquet
$ sudo install -o tessera -g tessera -m 0640 points.parquet /srv/tessera/corpus/
```

Then build into `bundle.next`, as the `tessera` user in `/srv/tessera`:

```console
tessera$ tessera build --carry-id-key-from bundle --bump-idset --out bundle.next
view 'ireland': web_mercator, quantising against x [0.46875, 0.5], y [0.3125, 0.34375]
        asked for lon [-11, -5], lat [51, 55.5] — snapped outward to the square at z5 (15, 10), lon [-11.25, 0], lat [48.922499263758255, 55.7765730186677]
        the data spans x [0.47030425, 0.48405091666666666], y [0.3141878520203559, 0.33315262108750776] — 28830 x 39773 of the 65536 x 65536 cells
        29935 point(s) placed, none on the frame's edge
        none of them outside web_mercator's ±85.0511287798066° domain, so nothing was clipped
...
built bundle.next (v00000): 29935 items, 1 terms, 29935 pairs, 2181449 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

A build never writes over an existing bundle, which is why it needs `--out`. Without it:

```text
build FAILED: invalid input: /srv/tessera/bundle already contains a bundle (CURRENT exists); remove it or choose another --out
```

## Swap it in

Stop the server, put the new bundle in place, and move the old log and cache aside with the old
bundle:

```console
$ sudo systemctl stop tessera
tessera$ mv bundle bundle.old && mv bundle.next bundle
tessera$ mv state/wal state/wal.old && mv state/cache state/cache.old && mkdir state/wal
$ sudo systemctl start tessera
```

The old log belongs to the old bundle. Its records name items by the old bundle's internal numbers,
which after this rebuild may belong to other items. Nothing checks which bundle a log was written
against, so a deletion or suppression replayed over the new bundle could hide the wrong item. The
cache is tied to the bundle it was built from, and moving it aside only frees the space.

The new bundle holds exactly what the sources hold. Anything ingested, deleted, suppressed or
declared through the running server since the last build is not in it, so put those changes into
the sources before you rebuild, or send them again now.

Check that the server is ready and that a `tessera_id` sent with the old idset is refused:

```console
tessera$ curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9151/readyz
200
tessera$ TOKEN=$(curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.secret)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}" | jq -r .token)
tessera$ curl -sS http://127.0.0.1:9151/v1/meta -H "authorization: Bearer $TOKEN" | jq .idset
2
tessera$ curl -sS http://127.0.0.1:9151/v1/items/1 \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"idset": 1}'
{"error":"conflict","detail":"stale idset; re-resolve by external_id"}
```

Delete the three `.old` directories once the new bundle serves what you expect.

## Keep a copy of the key

`--carry-id-key-from` needs the bundle. If the bundle is lost, the key has to come from somewhere
else, so keep a copy of `.env` apart from the bundle's backups. `tessera build --identity-file`
reads the key from a file in this form, which can also hold the idset:

```toml
[identity]
key   = "<32 hex characters>"
idset = 2
```

If you lose both the bundle and the key, the only build left is `--mint-id-key`, which starts a new
key. Every `tessera_id` anyone has saved then names a different item or none.

When a build is given two keys that differ, it refuses and prints a short fingerprint of each, so
you can tell which one is the odd one out:

```text
build refused: identity key sources disagree (--carry-id-key-from=fp:8d6fd356, --identity-file=fp:a0f851bb, /srv/tessera/.env (TESSERA_IDENTITY_KEY)=fp:8d6fd356); pass --rotate-id-key to confirm the rotation — this invalidates every tessera_id any client holds and reorders every row, since the key is now part of the storage sort key
```

Pass `--rotate-id-key` only when you mean to change the key. [Key
rotation](../system/access-control.md#key-rotation) says what a client sees afterwards.
