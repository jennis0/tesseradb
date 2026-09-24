# Rebuild and replace the bundle

## The identity key and rebuilds

Inside the server every item has an internal number, which never leaves it. A browser gets a
`tessera_id` instead, which is that number scrambled with the identity key. The [security
chapter](../system/security.md#a-client-never-sees-an-entity-id) explains why.

`tessera build` needs the key, and writes it into the bundle. The server reads it from there and
needs nothing else. What matters is the next build. A `tessera_id` that a viewer saved, or put in a
link, only goes on meaning the same item if every later build uses the same key.

Rebuild with `--carry-id-key-from`, naming the bundle you are serving. It reads the key from that
bundle, along with two things `.env` does not hold. One is the idset, a counter a client can compare
to tell whether a `tessera_id` it saved still means what it did. The other is the batch size, if the
first build sorted the corpus in batches, which decides the internal number each item gets. A build
never writes over a bundle, so give it a new directory:

```console
tessera$ tessera build --carry-id-key-from bundle --out bundle.next 2>&1 | tail -2
built bundle.next (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

Without `--out` it refuses:

```text
build FAILED: invalid input: /srv/tessera/bundle already contains a bundle (CURRENT exists); remove it or choose another --out
```

Keep a copy of `.env` somewhere other than the bundle's backups. You need it only if the bundle is
lost. `tessera build --identity-file` reads the key from a file instead of `.env`. That file can
hold the idset as well, which matters once your deployment has moved past idset 1:

```toml
[identity]
key   = "<32 hex characters>"
idset = 1
```

If you lose both the bundle and the key, the only build left is `--mint-id-key`, which starts a new
key, and every `tessera_id` anyone has saved then names a different item or none.

When the build is given two keys that differ, it refuses and prints a short fingerprint of each so
you can tell which is the odd one out:

```text
build refused: identity key sources disagree (--carry-id-key-from=fp:8d6fd356, --identity-file=fp:74063467, /srv/tessera/.env (TESSERA_IDENTITY_KEY)=fp:8d6fd356); pass --rotate-id-key to confirm the rotation — this invalidates every tessera_id any client holds and reorders every row, since the key is now part of the storage sort key
```

Pass `--rotate-id-key` only when you mean to change the key. [Key
rotation](../system/access-control.md#key-rotation) says what a client sees afterwards.

## Replace the bundle with a rebuilt one

You rebuild when the sources have changed in ways easier to build than to send as changes, or when a
new version of Tessera cannot read the old bundle. A rebuilt bundle holds exactly what the sources
hold. Anything ingested, deleted, suppressed or declared through the running server since the last
build is missing from it, so put those changes into the sources first, or send them again once the
new bundle is served.

Build into a new directory while the server keeps running, then stop it, put the new bundle in
place, and move the old log and cache aside with the old bundle:

```console
tessera$ tessera build --carry-id-key-from bundle --out bundle.next 2>&1 | tail -2
built bundle.next (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
$ sudo systemctl stop tessera
tessera$ mv bundle bundle.old && mv bundle.next bundle
tessera$ mv state/wal state/wal.old && mv state/cache state/cache.old && mkdir state/wal
$ sudo systemctl start tessera
tessera$ curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9151/readyz
200
```

The old log belongs to the old bundle. Its records name items by the internal numbers the old bundle
gave them, and a rebuild numbers the items afresh. Nothing checks which bundle a log was written
against, so a deletion or suppression replayed over the new bundle would hide whichever item now has
that number. The cache is tied to the bundle it was built from and the new bundle never reads it, so
moving it aside only frees the space. Delete the three `.old` directories once the new bundle serves
what you expect.

Under Docker the steps are the same, with the move done by another container; [Replace the
bundle](docker.md#replace-the-bundle) in the Docker guide shows them.
