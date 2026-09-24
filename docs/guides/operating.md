# Operate a deployment

Once Tessera is running, [under systemd](systemd.md) or [with Docker](docker.md), some decisions are
still yours. Settle who may reach each address, how much memory the server gets and how browsers
reach it over TLS before anyone else uses it. The later sections, on health, failed writes,
compaction and rebuilds, are the ones you'll come back to.

The examples use the systemd guide's layout, with commands after `tessera$` run as the `tessera`
user in `/srv/tessera` and the control address on a socket. With Docker, send the same requests to
the ports you published, 9161 to 9163 in the Docker guide, and read the credentials from `secrets/`.

## What the server keeps on disc

`[bundle]` in `tessera.toml` names three places, and they need different care.

| Key | What it holds | What makes it grow | If it is lost |
|---|---|---|---|
| `path` | The bundle: every item, its position, its attributes, the search indexes and the identity key. The server writes to it as it runs, adding a segment each time it makes newly ingested items visible and rewriting it at each compaction | Ingest, until a compaction reclaims what deletions and merges left behind | The database. Restore it from a backup |
| `wal` | The write-ahead log. Every change since the last flush into the bundle, in files named after this path: `wal-000001.log`, `wal-000001.sync` and so on | Writes, until the bundle holds them and the oldest files are deleted | Every change acknowledged since the last flush |
| `cache` | Work the server can redo: each distinct grant's set of visible items, the name-search suggestions and scratch files | A pair of files for each distinct set of labels a viewer has been granted. A compaction deletes them | Nothing but time. The server rebuilds what it needs |

Back up the bundle and the log together, with the server stopped. A copy of one without the other is
a database as it stood at some other moment.

!!! warning
    Keep the bundle and the log on a local disc. The server locks the bundle's directory so that a
    second server cannot write to it, and that lock does not work reliably over NFS or SMB. Two
    servers writing one bundle overwrite each other's changes.

Anyone who can read the bundle can read every item in it, whatever its access label, and the
identity key too. Access control applies to what the server sends, not to the files.

## Who reaches each address

The server listens on three addresses, and each is meant for a different caller.

| Address | Routes | Who should reach it |
|---|---|---|
| Viewer | `/v1/*`, `/healthz`, `/readyz` | Browsers, each with its own token, through a TLS proxy |
| Session | `/session/authorise`, `/session/revoke`, `/healthz`, `/readyz` | Your application's backend, which signs people in and asks Tessera for their tokens |
| Control | `/control/*` | You, and whatever loads data into the corpus |

The session address hands out tokens to anyone holding the session credential, and the control
address accepts deletions and new data from anyone holding the operator credential. Neither should
be reachable from the internet. The viewer address serves only what each token allows, and is the
one browsers need.

The server reads each credential once, when it starts, and trims the whitespace around it. To change
one, replace the file and restart the server. Instead of a file, `session_credential_env` and
`operator_credential_env` can name an environment variable. A file is the better choice, since a
process's environment can be read from `/proc` by anything else running as the same user. The server
wants both credentials even if nothing ever calls the control address:

```text
tessera serve: refused to start: there is no operator credential; set `operator_credential_file` or `operator_credential_env` under [serve] and put the secret in that file or variable
```

!!! warning
    Not built yet: a plugin that checks a claim against your identity provider.
    `builtin:passthrough` is the only plugin, and it grants whatever labels the caller of
    `/session/authorise` asks for. The session credential is therefore all that stands between a
    caller and every item in the corpus. Keep the session address away from browsers and the
    network, and have your backend decide each person's labels from its own sign-in.
    [Deployment](../system/clients.md#deployment) in the clients chapter shows both ways this goes
    wrong.

A browser's token lasts `token_max_lifetime` seconds and keeps the labels it was issued with. If you
take a label away from someone, they keep seeing its items until their token expires, unless your
backend revokes it with `/session/revoke`.

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

## Memory

The server holds five caches, each with a limit you can set under `[serve]`:

| Key | Default | What it holds |
|---|---|---|
| `row_projection_cache_bytes` | 2 GiB | For each session, which rows of the bundle it may see |
| `fragment_cache_bytes` | 1 GiB | For each distinct set of granted labels, the items it covers. Copies on disc in the cache directory outlive the memory |
| `masked_count_cache_bytes` | 256 MiB | Visible member counts for annotation layers stored by row. Empty in a corpus without one |
| `region_cache_bytes` | 256 MiB | Drawn filter regions broken into map tiles, shared between viewers |
| `occupancy_cache_bytes` | 32 MiB | How many tiles at each zoom hold something a session can see |

Paged reads of records, `POST /v1/items`, hold up to seven pages each while they run. Two may run at
once (`bulk_admission`), and a page is up to 64 MiB (`max_page_bytes`). The server logs what that
comes to when it starts, as `bulk_read_memory_bytes=939524096`. With the defaults, the caches and
paged reads come to about 4.4 GiB.

The bundle itself is not loaded. The server maps its files, so their pages sit in the kernel's page
cache and count towards the process's memory, but the kernel drops them when memory is short and
reads them back from disc when a request needs them. Set the cap (`MemoryMax` under systemd, or a
memory limit on the container) above the caches and paged reads together. Every gigabyte beyond that
keeps more of the bundle in memory.

`/control/status` shows what the process holds. `anon_bytes` is the caches and the allocator's own
memory, and `file_bytes` is how much of the bundle is in memory:

```console
tessera$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  http://localhost/control/status | jq '{posture: .write_executor.posture, heap}'
{
  "posture": "running",
  "heap": {
    "anon_bytes": 5169152,
    "file_bytes": 20275200,
    "last_trim_micros": 0,
    "last_trim_returned_bytes": 0,
    "resident_bytes": 25444352,
    "trim_baseline_bytes": 4808704,
    "trim_growth_bytes": 268435456,
    "trims": 0
  }
}
```

`compute_threads` defaults to the number of CPUs the process may use. Lower it to leave some for
other work on the same machine.

A build sizes itself separately. `tessera build --memory-budget 24g` keeps the build's own
structures within 24 GiB; without it the build works out a budget from the memory the machine has
free. If you rebuild on the server's machine while it runs, give a budget that leaves the server its
share.

## Put nginx in front of the viewer address

Browsers should reach the viewer address over TLS, and from the same origin as the page that shows
the map. A reverse proxy does both. This nginx configuration serves your application and passes
`/v1/` to Tessera:

```nginx
server {
    listen 443 ssl http2;
    server_name maps.example.org;

    ssl_certificate     /etc/ssl/maps.example.org/fullchain.pem;
    ssl_certificate_key /etc/ssl/maps.example.org/privkey.pem;

    location /v1/ {
        proxy_pass http://127.0.0.1:9151;
        proxy_http_version 1.1;
        proxy_buffering off;
        client_max_body_size 2m;
    }

    location / {
        proxy_pass http://127.0.0.1:8080;
    }
}
```

Here `127.0.0.1:8080` is your application, and `127.0.0.1:9151` is the viewer address (under the
Docker guide, `127.0.0.1:9161`). Only `/v1/` reaches Tessera, so the session address and the health
checks stay private.

A map response arrives in pieces: the counts for each tile first, then points as they are found,
then a final frame. `proxy_buffering off` has nginx pass each piece on as it arrives, so the browser
can start drawing before the response is complete. The viewer address refuses a request body over 2
MiB, and `client_max_body_size 2m` has nginx refuse at the same size. By default nginx gives up
after 60 seconds without hearing from Tessera, and Tessera ends any response that runs longer than
60 seconds (`serve.stream_deadline_ms`), so the default suits it.

The proxy must pass on every response header. nginx does unless you tell it otherwise, so add no
`proxy_hide_header` for any of these:

- `etag`
- `x-tessera-identity-key`
- `x-tessera-pin`
- `x-tessera-stale`
- `x-tessera-server-us`
- `x-tessera-admission-us`
- `x-tessera-region`
- `retry-after`, sent with a 429 when the server is busy

The browser client keeps what it has already been sent, filed under `x-tessera-identity-key`, and
reads `etag`, `x-tessera-pin` and `x-tessera-stale` to tell whether it is out of date. Without the
first, one person's cached map could be shown to the next person who signs in on the same page. To
check the headers come through:

```console
tessera$ TOKEN=$(curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.cred)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}" | jq -r .token)
tessera$ curl -sS -o /dev/null -D - https://maps.example.org/v1/viewport \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"view": "ireland", "zoom": 0, "tiles": [0], "k": 0}'
HTTP/2 200 
server: nginx/1.18.0 (Ubuntu)
date: Thu, 24 Sep 2026 08:48:15 GMT
content-type: application/octet-stream
x-tessera-identity-key: 27d1f8b6d017ae6de0a6031d7895b89a
x-tessera-server-us: 12644
x-tessera-admission-us: 1
etag: "b6c42bd9060e5db3a6d7275e85d806c0"
x-tessera-pin: {"prefix":"v00000","segments_version":0}
x-tessera-stale: 0
```

### A page on another origin

If the page with the map comes from a different origin, such as `https://www.example.org` calling
the viewer at `https://maps.example.org`, the browser will only pass the response on if Tessera
names the page's origin. List it under `[serve]`:

```toml
cors_origins = ["https://www.example.org"]
```

The list applies to the viewer address only, and a page on those origins still cannot call the
session address. A wildcard is refused:

```text
tessera serve: refused to start: serve.cors_origins contains "*", which a CORS origin list cannot hold; list each origin, such as "https://app.example", or remove the key
```

Leave `dev_cors_origins` unset outside development. It opens the session address to the pages it
lists as well as the viewer, and the server logs a warning when it is set. `cors_loopback = true`
lets any page served from `localhost`, `127.0.0.1` or `[::1]` use the viewer address, which is what
a notebook needs.

## Health and readiness

The viewer and session addresses both answer `/healthz` and `/readyz`, with no credential and no
body. `/healthz` answers 200 whenever the process is answering at all. `/readyz` answers 200 while
the part of the server that takes writes is running normally, and 503 otherwise. That is the one to
send traffic by. The control address has neither, and turns away a request without the operator
credential.

The nginx configuration above does not pass the probes through, so run your load balancer's or
monitor's check on the server's machine, or against the session address from inside your network.
`tessera health` does the same check from the command line. It reads `tessera.toml`, asks the viewer
address for `/readyz`, and exits 0 on a 200. The Docker image uses it as its health check.

```console
tessera$ tessera health && echo ready
ready
```

When nothing is listening it says so and exits 1:

```text
tessera health: no server answering at 127.0.0.1:9151: Connection refused (os error 111)
```

`/control/status` names the state of the write side in `write_executor.posture`: `not-started`,
`running`, `wal-poisoned` or `dead`. Only `running` is ready.

A disc that hangs on a write, rather than failing it, leaves the posture at `running` and `/readyz`
at 200 while nothing is written. [Health and readiness](../system/serving.md#health-and-readiness)
covers what the probe can and cannot see.

## When a write to the log fails

Every change is written to the log and flushed to disc before the caller is told it succeeded. What
happens when that fails depends on which step failed.

If appending to the log fails, the posture becomes `wal-poisoned` and `/readyz` answers 503 until
the server restarts.

If the record is written but forcing it to disc fails, the server tries again a few times. If it
still fails, it throws away what did not reach the disc and goes back to `running`, and `/readyz`
answers 200 again. The only sign in `/control/status` is `write_executor.wal_recoveries` going up by
one. It is not back to normal, though. From then on it makes no newly ingested items visible, never
trims the log, and refuses every compaction with the reason `overlay_diverged`. The log says so in a
line beginning `ALARM` and ending "Restart this node."

Either way, a deletion or suppression whose write failed is still applied, in memory, so the item
stays hidden. The caller is answered 500, with a detail that includes "a change in it may be in
force, so do not treat this as a no-op". An unsuppress whose write failed is not applied. The
deletions and suppressions that were applied have no record on disc, so a restart forgets them.

!!! warning
    A restart can bring a deleted or suppressed item back into view, if the change that hid it was
    answered 500. After a failed write:

    1. Have whatever sends changes to `/control/changes` send every change that was answered 500
       again.
    2. Fix the disc.
    3. Restart the server.
    4. Send every change that was answered 500 again.

    Restart the server on its process exiting, never on `/readyz` answering 503. The systemd unit
    and the Compose file in these guides both do that.

## Compaction

The server adds a segment to the bundle each time it makes newly ingested items visible, and merges
small segments as they build up. Deleted items stay in the bundle, hidden from every viewer, until a
compaction rewrites the bundle as one segment per view without them. Compaction is also what
reclaims the disc space merges leave behind.

The server starts one when any of these conditions holds, each set under `[ingest]`:

| Key | Default | Starts a compaction when |
|---|---|---|
| `compaction_window_start`, `compaction_window_secs`, `compaction_window_min_segments` | `"00:00"`, 14400, 8 | A view has 8 segments or more during the four hours from midnight UTC |
| `compaction_max_segments` | 64 | A view has 64 segments, at any hour |
| `compaction_after_deletions` | 500000, the value of `overlay_soft_limit` | 500,000 deleted items are waiting to be removed |
| `compaction_dead_rows_fraction` | 0.2 | Deleted items waiting to be removed are at least a fifth of all rows |
| `compaction_dead_bytes_ratio` | 1.0 | Files the bundle no longer uses take at least as much space as the files it does |

Set `compaction_window_start` to the start of your quietest hours, in UTC as `"HH:MM"`. Requests are
answered as usual while a compaction runs, but when it finishes every session's cached view of the
map is out of date at once. A background pass rebuilds them, and a session it has not reached yet
rebuilds its own on its next request, which makes that request slower.

After one compaction starts, none of these conditions starts another until
`compaction_min_interval_secs` has passed, 86400 seconds by default, and never while one is still
running. The server keeps that time in memory, so after a restart the first condition that holds
starts one straight away. A compaction that was refused does not count. Writing `"off"` for
`compaction_window_start`, `compaction_max_segments`, `compaction_after_deletions`,
`compaction_dead_rows_fraction` or `compaction_dead_bytes_ratio` turns that condition off and leaves
the others.

A compaction needs free space on the bundle's disc of one and a half times the bundle's current
size, and enough memory for its own estimate, which respects the cap you set. Without either it is
refused, and `/control/status` records the reason under `compaction.last_refusal`.

To start one yourself, whatever the schedule says:

```console
tessera$ curl -sS -X POST --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  -w '%{http_code}\n' http://localhost/control/compact
202
```

The server picks the request up at its next tick. A request made while a compaction is running is
dropped, with a warning in the log. Read the outcome a few seconds later:

```console
tessera$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  http://localhost/control/status | jq '.compaction | {folds, fold_refusals, last_refusal}'
{
  "folds": 1,
  "fold_refusals": 0,
  "last_refusal": null
}
```

[Compaction](../system/write-path.md#compaction) in the write-path chapter describes it in full.

## Restarts

The server keeps sessions in memory, so a restart ends every one of them, and each browser has to
ask your backend for a new token. When it starts, the server opens the newest state of the bundle
recorded on disc that passes its checks, and replays the log over it, so every change it
acknowledged before it stopped is back. [Restart and
recovery](../system/write-path.md#restart-and-recovery) has the detail.

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
