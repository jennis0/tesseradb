# Rebuild and replace the bundle

You rebuild the bundle when the sources have changed in ways that are easier to build than to send
to the running server, or when a new version of Mosaica can't read the old bundle. The new bundle
goes into a directory of its own while the server keeps running, and replaces the old one in a
short stop.

A rebuild changes the `mosaica_id` a browser uses to name every item. The examples use [the
systemd install](systemd.md), and the [Docker guide](docker.md#replace-the-bundle) shows the swap
under Compose.

## What a rebuild changes

Inside the server every item has an internal number, which never leaves it. A browser gets a
`mosaica_id` instead, which is the internal number scrambled with a key. The [security
chapter](../system/security.md#a-client-never-sees-an-entity-id) explains why. `mosaica build`
draws a new random key each time it creates a bundle and stores it in the bundle, where the server
reads it. You never see or supply the key.

So a rebuild gives every item a new `mosaica_id`, even when the sources have not changed, and a
`mosaica_id` saved from the old bundle does not name an item in the new one. A client that holds
`mosaica_id`s has to read those items again after the swap, by a unique field's values or with a
fresh read. A copy of a bundle, such as one restored from a backup, keeps its key and so keeps every
`mosaica_id`.

## Build the new bundle

The tutorial found Saint Patrick's Bridge in Germany, because GeoNames lists its longitude as
8.47036 where it should be −8.47036. Correct the line in `IE.txt`, convert it again, and copy the
new Parquet file into place:

```console
$ cd ~/ireland
$ awk -F'\t' -v OFS='\t' '$1 == 3302004 { $6 = "-8.47036" } { print }' IE.txt > IE.fixed && mv IE.fixed IE.txt
$ .venv/bin/python convert.py
wrote 29935 places to points.parquet
$ sudo install -o mosaica -g mosaica -m 0640 points.parquet /srv/mosaica/corpus/
```

Then build into `bundle.next`, as the `mosaica` user in `/srv/mosaica`:

```console
mosaica$ mosaica build --out bundle.next
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
build FAILED: invalid input: /srv/mosaica/bundle already contains a bundle (CURRENT exists); remove it or choose another --out
```

## Swap it in

Stop the server, put the new bundle in place, and move the old log and cache aside with the old
bundle:

```console
$ sudo systemctl stop mosaica
mosaica$ mv bundle bundle.old && mv bundle.next bundle
mosaica$ mv state/wal state/wal.old && mv state/cache state/cache.old && mkdir state/wal
$ sudo systemctl start mosaica
```

The old log belongs to the old bundle. Its records name items by the old bundle's internal numbers,
which after this rebuild may belong to other items. Nothing checks which bundle a log was written
against, so a deletion or suppression replayed over the new bundle could hide the wrong item. The
cache is tied to the bundle it was built from, and moving it aside only frees the space.

The new bundle holds exactly what the sources hold. Anything ingested, deleted, suppressed or
declared through the running server since the last build is not in it, so put those changes into
the sources before you rebuild, or send them again now.

Check that the server is ready:

```console
mosaica$ curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9151/readyz
200
```

Delete the three `.old` directories once the new bundle serves what you expect.

