# Run a standalone deployment

Run Tessera on one Linux server as a systemd service, with nginx in front of the viewer plane for
TLS. You need the `tessera` binary, the corpus declaration and sources you built your bundle
from, and the identity key that build used. The examples serve the GeoNames extract for Ireland.

!!! note ""
    Not built yet: a package, an install script or a container image. Copy the binary to
    `/usr/local/bin` by hand, as the first step shows.

## Lay out the server

Give the service a user of its own and one directory tree:

```bash
sudo useradd --system --home-dir /srv/tessera --shell /usr/sbin/nologin tessera
sudo install -m 0755 tessera /usr/local/bin/tessera
sudo install -d -o tessera -g tessera -m 0750 /srv/tessera /srv/tessera/corpus /srv/tessera/state /srv/tessera/state/wal
sudo install -d -o tessera -g tessera -m 0700 /srv/tessera/secrets
```

Copy the declaration and its Parquet sources into `/srv/tessera/corpus`. The server keeps three
things on disc, each named in `[bundle]` in `tessera.toml`:

| Key | Path here | What it holds | What makes it grow | If it is lost |
|---|---|---|---|---|
| `bundle.path` | `bundle/` | The corpus: manifests, segments, the term index and the identity key. The server writes a new segment into it at each flush and a new prefix at each compaction | Ingest. A compaction reclaims what merges and deletions left behind | Everything is lost. Restore it from a backup |
| `bundle.wal` | `state/wal/wal.log` | Every ingest and deny not yet written into the bundle, as `wal-000001.log`, `wal-000001.sync` and so on beside the path you name | Writes since the last flush. Older members are deleted once the bundle holds their rows | Acknowledged writes and denies since the last flush are lost |
| `bundle.cache` | `state/cache/` | Each distinct grant's authorised set, the suggestion indexes and scratch files | A pair of files for each distinct set of granted terms, and a new pair when that set is authorised again after a flush. A compaction deletes them all | Nothing. The server rebuilds the suggestion indexes every time it starts, and an authorised set the next time a viewer is authorised with it |

The server creates the cache directory. It does not create the directory the WAL goes in, and
refuses to start without it:

```text
tessera serve: refused to start: wal error: wal io error: No such file or directory (os error 2)
```

!!! warning
    Keep the bundle and the WAL on a local filesystem. The server takes a `flock` on the bundle
    directory so that a second process cannot write the same bundle, and that lock is unreliable
    over NFS or SMB. Two writers overwrite each other's manifests.

Anyone who can read the bundle can read every item in it, whatever its access label, and the
identity key with them. Leave `/srv/tessera` readable by the `tessera` user only.

## Write tessera.toml

Put this in `/srv/tessera/tessera.toml`:

```toml
[bundle]
path  = "bundle"
cache = "state/cache"
wal   = "state/wal/wal.log"

[build]
schema = "corpus/corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:9151"
session = "127.0.0.1:9152"
control = "unix:/run/tessera/control.sock"
session_credential_file  = "secrets/session.cred"
operator_credential_file = "secrets/operator.cred"
```

Relative paths resolve against the directory `tessera.toml` is in, wherever the process was
started from. Every section refuses a key it does not know, so a misspelt key stops the server at
startup with a list of the keys that section takes.

`token_max_lifetime` has no default. It is how many seconds a viewer's token lasts. A token keeps
the terms it was issued with until it expires or is revoked, so this is also the longest a viewer
whose access has been narrowed goes on seeing what they saw before. `builtin:passthrough` is the
only plugin.

The keys this page does not cover keep their defaults. Each is a field of `RawServe` or
`RawIngest` in
[`crates/tessera-config/src/lib.rs`](https://github.com/jennis0/tesseradb/blob/main/crates/tessera-config/src/lib.rs),
and its default is in
[`defaults.rs`](https://github.com/jennis0/tesseradb/blob/main/crates/tessera-config/src/defaults.rs)
beside it.

## Decide who reaches each plane

The server listens on three planes and needs an address for each. None has a default.

| Plane | Routes | Who reaches it | Address here |
|---|---|---|---|
| Viewer | `/v1/*`, `/healthz`, `/readyz` | Browsers, through nginx | `127.0.0.1:9151` |
| Session | `/session/authorise`, `/session/revoke`, `/healthz`, `/readyz` | Your application's backend, which signs users in and asks for their tokens | `127.0.0.1:9152`, or a private address if the backend runs on another host |
| Control | `/control/*` | Operators and ingest jobs | `unix:/run/tessera/control.sock` |

The control plane takes a unix socket or a TCP address. Use the socket: the unit file below
leaves it readable by the `tessera` user only. The server does not check that a TCP control
address is a loopback one, and will listen on `0.0.0.0:9153` if you write that.

Every control route needs the operator credential, and every session route the session
credential. Create both:

```bash
cd /srv/tessera
sudo -u tessera sh -c 'umask 077; openssl rand -hex 32 > secrets/session.cred; openssl rand -hex 32 > secrets/operator.cred'
```

The server reads each file once, at startup, and trims the whitespace around it. To change a
credential, replace the file and restart. `session_credential_env` and `operator_credential_env`
name an environment variable instead of a file; prefer the file, since any process running as the
same user can read another's environment from `/proc`. The server needs both credentials even if
nothing ever calls the control plane:

```text
tessera serve: refused to start: there is no operator credential; set `operator_credential_file` or `operator_credential_env` under [serve] and put the secret in that file or variable
```

!!! warning
    Not built yet: a plugin that checks a claim against your identity provider.
    `builtin:passthrough` believes whatever terms the caller of `/session/authorise` names, so
    the session credential is all that stands between a caller and every item a term can name.
    Keep the session plane off the public network and out of every browser, and have your
    backend decide each user's terms from its own sign-in. The two ways this goes wrong are
    described under [Deployment](../system/clients.md#deployment) in the clients chapter.

## Keep the identity key

The identity key turns each item's internal id into the `tessera_id` clients see. `tessera build`
needs it, and writes it into the bundle's manifest. `tessera serve` reads it from the bundle and
needs nothing else.

Your first build read the key from `TESSERA_IDENTITY_KEY` or a `.env` file beside `tessera.toml`,
or printed it if you passed `--mint-id-key`. Put the same 32 hex characters in a file on the
server:

```bash
sudo -u tessera sh -c 'umask 077; printf "[identity]\nkey = \"%s\"\n" "<your key>" > /srv/tessera/secrets/identity.toml'
```

Build as the `tessera` user, so the service can write into the bundle:

```console
$ cd /srv/tessera
$ sudo -u tessera tessera build --identity-file secrets/identity.toml
...
built /srv/tessera/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2255110 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

A build never writes over an existing bundle:

```text
build FAILED: invalid input: /srv/tessera/bundle already contains a bundle (CURRENT exists); remove it or choose another --out
```

Keep a copy of `identity.toml` away from the bundle's backups. If you lose the file and still
have the bundle, `--carry-id-key-from /srv/tessera/bundle` reads the key back out of it on the
next build. If you lose both, the only build left is `--mint-id-key`, which starts a new key, and
every `tessera_id` a client has saved or shared then names a different item or none.

A build given two keys that differ refuses:

```text
build refused: identity key sources disagree (--carry-id-key-from=fp:36f412fb, --identity-file=fp:9ca3dd3a); pass --rotate-id-key to confirm the rotation — this invalidates every tessera_id any client holds and reorders every row, since the key is now part of the storage sort key
```

Pass `--rotate-id-key` only when you mean to change the key. A new key takes effect when the
server restarts on the new bundle; [key rotation](../system/access-control.md#key-rotation) says
what a client sees.

## Start the server and check it

Run it once by hand as the `tessera` user. Under systemd the unit creates `/run/tessera` for the
control socket; for this run, create it yourself:

```console
$ sudo install -d -o tessera -g tessera -m 0700 /run/tessera
$ cd /srv/tessera
$ sudo -u tessera tessera serve
2026-09-23T23:48:40.844894Z  INFO tessera_server::memory: the allocator's arena count is capped arenas=12
2026-09-23T23:48:40.875765Z  INFO tessera_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-23T23:48:40.876090Z  INFO tessera_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9151","session":"127.0.0.1:9152","control":"unix:/run/tessera/control.sock"}
```

The log goes to standard error. Standard output carries one JSON line, written once all three
planes are listening, which a supervisor can wait for. If the socket's directory is missing, the
server stops with an error that does not name the path:

```text
tessera serve: No such file or directory (os error 2)
```

From a second shell in `/srv/tessera`, probe the viewer plane:

```console
$ curl -sSi http://127.0.0.1:9151/healthz
HTTP/1.1 200 OK
content-length: 0
date: Wed, 23 Sep 2026 23:48:40 GMT

$ curl -sSi http://127.0.0.1:9151/readyz
HTTP/1.1 200 OK
content-length: 0
date: Wed, 23 Sep 2026 23:48:40 GMT
```

Ask the session plane for a token, as your backend will. Under `builtin:passthrough`, `auth_data`
is base64 of a JSON object listing the terms to grant:

```console
$ curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.cred)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}"
{"token":"fae810fb9a159ba9bafa9ed60bcd47360799dc172d46c38d80ccdcee8c0dca25","token_id":0,"expires_at":1790210920}
```

Without the credential the session plane refuses:

```console
$ curl -sS http://127.0.0.1:9152/session/authorise \
  -H 'content-type: application/json' \
  -d '{"auth_data": "e30="}'
{"error":"bad-credential","detail":"missing or invalid bearer credential"}
```

Stop the server with Ctrl-C.

## Size the memory

The process holds five caches, each bounded by a key under `[serve]`:

| Key | Default | What it holds |
|---|---|---|
| `row_projection_cache_bytes` | 2 GiB | Each session's visible items, arranged by row |
| `fragment_cache_bytes` | 1 GiB | The authorised set for each distinct grant. Copies on disc in the cache directory outlive the memory |
| `masked_count_cache_bytes` | 256 MiB | Visible member counts for annotation layers stored by row. Empty in a corpus without one |
| `region_cache_bytes` | 256 MiB | Drawn filter regions broken into tiles, shared between viewers |
| `occupancy_cache_bytes` | 32 MiB | How many tiles at each depth hold something a session can see |

Bulk reads of records, `POST /v1/items`, hold up to seven pages each: `bulk_admission` reads at
once (default 2) times `max_page_bytes` (default 64 MiB) times seven. The server logs that figure
at startup, 939524096 bytes with the defaults.

The bundle is mapped from disc rather than loaded. Its pages sit in the kernel's page cache and
count towards the service's memory, but the kernel drops them under pressure, and a request that
needs a dropped page reads it from disc again. Set the unit's `MemoryMax` above the caches and bulk
reads together, and every gigabyte above that keeps more of the bundle in memory. With the defaults
the caches and bulk reads come to about 4.4 GiB.

`/control/status` reports what the process holds. `anon_bytes` is the caches and the allocator's
own memory, and `file_bytes` is the bundle's resident pages:

```console
$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  http://localhost/control/status | jq '{posture: .write_executor.posture, heap}'
{
  "posture": "running",
  "heap": {
    "anon_bytes": 7249920,
    "file_bytes": 18984960,
    "last_trim_micros": 0,
    "last_trim_returned_bytes": 0,
    "resident_bytes": 26234880,
    "trim_baseline_bytes": 6967296,
    "trim_growth_bytes": 268435456,
    "trims": 0
  }
}
```

`compute_threads` defaults to the number of CPUs the process may use, and also caps the
allocator's arenas, which the first log line reports.

A build sizes itself separately. `tessera build --memory-budget 24g` bounds the build's own
structures at 24 GiB; without the flag it derives a budget from the machine's available memory.
If you rebuild on the serving host while the service runs, give a budget that leaves the service
its share.

## Run it under systemd

Put this in `/etc/systemd/system/tessera.service`:

```ini
[Unit]
Description=Tessera map server
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=tessera
Group=tessera
WorkingDirectory=/srv/tessera
ExecStart=/usr/local/bin/tessera serve --deployment /srv/tessera/tessera.toml
Environment=NO_COLOR=1
Restart=on-failure
RestartSec=5
MemoryMax=24G
MemorySwapMax=0
UMask=0077
RuntimeDirectory=tessera
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=/srv/tessera/bundle /srv/tessera/state

[Install]
WantedBy=multi-user.target
```

Then start it and follow its log:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now tessera
journalctl -u tessera -f
```

`NO_COLOR=1` stops the log carrying colour escape codes, which the server writes even when
standard error is not a terminal. The server ignores `RUST_LOG`; it logs at `INFO` and above.
`RuntimeDirectory` creates `/run/tessera` for the control socket, and `UMask=0077` leaves the
socket and every file the server writes readable by the `tessera` user only. Set `MemoryMax` as
the previous section describes; `MemorySwapMax=0` keeps the process out of swap when it reaches
that cap. `ProtectSystem=strict` makes the whole filesystem read-only to the service except the paths in
`ReadWritePaths`, the runtime directory and a private `/tmp`.

`systemctl stop` sends `SIGTERM`, and the server exits at once without draining. Every write it
acknowledged is already in the WAL, and a viewer whose response was cut off receives it without
its final frame, which the client treats as incomplete.

Sessions are held in memory, so a restart ends every one of them and each client asks your
backend for a new token. On start the server opens the newest manifest that verifies and replays
the WAL over it, so every write it acknowledged before stopping is served again;
[restart and recovery](../system/write-path.md#restart-and-recovery) has the detail.

## Put nginx in front of the viewer plane

Serve the page that embeds the map and the viewer plane from one origin, with nginx terminating
TLS:

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

Here `127.0.0.1:8080` is your application. Only `/v1/` reaches Tessera, so the session plane and
the health probes stay private.

A viewport response streams: counts first, then points as they are found, then a final frame.
`proxy_buffering off` passes each frame on as it arrives rather than when nginx's buffer fills.
The viewer plane refuses a request body over 2 MiB, and `client_max_body_size 2m` makes nginx
refuse at the same size. nginx's default `proxy_read_timeout` of 60 seconds matches
`serve.stream_deadline_ms`, the longest a response may take, so leave it unless you raise that key.

Forward every response header. nginx does unless told otherwise, so do not add
`proxy_hide_header` for any of these:

- `etag`
- `x-tessera-identity-key`
- `x-tessera-pin`
- `x-tessera-stale`
- `x-tessera-server-us`
- `x-tessera-admission-us`
- `x-tessera-region`
- `retry-after`, on a 429

The client partitions what it has cached by `x-tessera-identity-key`. Without it, one viewer's
cached map can be shown under another viewer's token. Check the headers arrive through the proxy:

```console
$ TOKEN=$(curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.cred)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}" | jq -r .token)
$ curl -sS -o /dev/null -D - https://maps.example.org/v1/viewport \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"view": "ireland", "zoom": 0, "tiles": [0], "k": 0}'
HTTP/2 200 
server: nginx/1.18.0 (Ubuntu)
date: Wed, 23 Sep 2026 23:48:41 GMT
content-type: application/octet-stream
x-tessera-identity-key: 391f99b3821640a657cdb33ad42a95c0
x-tessera-server-us: 260
x-tessera-admission-us: 1
etag: "a5d847f8b680fc66cafdf742d5be13a0"
x-tessera-pin: {"prefix":"v00000","segments_version":0}
x-tessera-stale: 0
```

### Allow a page on another origin

If the page that embeds the map comes from another origin, such as `https://www.example.org`
calling the viewer plane at `https://maps.example.org`, list the page's origin under `[serve]`:

```toml
cors_origins = ["https://www.example.org"]
```

`cors_origins` applies to the viewer plane only, and the session plane refuses a browser's
cross-origin request from those origins. A wildcard is refused:

```text
tessera serve: refused to start: serve.cors_origins contains "*", which a CORS origin list cannot hold; list each origin, such as "https://app.example", or remove the key
```

Leave `dev_cors_origins` unset in production. It opens the session plane to the origins it lists
as well as the viewer plane, and the server logs a warning at startup when it is set.
`cors_loopback = true` admits any page served from `localhost`, `127.0.0.1` or `[::1]` on the
viewer plane, for notebooks.

## Watch health and readiness

Both probes are on the viewer and session planes, need no credential and return no body. The
control plane has neither; a request to it without the operator credential gets a 401.

`/healthz` answers 200 whenever the process is answering. `/readyz` answers 200 while the write
side is running and 503 otherwise, which includes before it starts, after it stops, and after a
write to the WAL has failed to reach the disc. `/control/status` names the state in
`write_executor.posture`: `not-started`, `running`, `wal-poisoned` or `dead`.

!!! warning
    Do not restart the server because `/readyz` answers 503. After a failed WAL write the server
    still applies every deletion and suppression it accepts, and a restart discards the ones that
    never reached the disc, which can bring a suppressed item back into view. Point your load
    balancer's check at `/readyz` on the viewer plane, and use `Restart=on-failure`, which acts
    only when the process exits.

A WAL write that hangs on a stalled disc rather than failing leaves the posture at `running` and
`/readyz` at 200. [Health and readiness](../system/serving.md#health-and-readiness) covers what
the probe can and cannot see.

## Schedule compaction

A compaction rewrites the bundle as one segment per view, removes the rows of deleted items, and
reclaims disc space that merges have left behind. Nothing else in the server does any of the
three.
The server starts one when any of the conditions below is met, each set under `[ingest]`:

| Key | Default | Starts a compaction when |
|---|---|---|
| `compaction_window_start`, `compaction_window_secs`, `compaction_window_min_segments` | `"00:00"`, 14400, 8 | Inside the four hours from midnight UTC, a view has 8 segments or more |
| `compaction_max_segments` | 64 | A view has 64 segments, at any hour |
| `compaction_after_deletions` | 500000, the value of `overlay_soft_limit` | 500,000 deleted items are waiting to be removed |
| `compaction_dead_rows_fraction` | 0.2 | Deleted items waiting to be removed are at least a fifth of all rows |
| `compaction_dead_bytes_ratio` | 1.0 | Files no manifest names take at least as many bytes as the files the manifests name |

None of these conditions starts a compaction within `compaction_min_interval_secs` of the
previous one starting, 86400 seconds by default. `compaction_window_start` takes a UTC time as
`"HH:MM"`. Writing `"off"` for `compaction_window_start`, `compaction_max_segments`,
`compaction_after_deletions`, `compaction_dead_rows_fraction` or `compaction_dead_bytes_ratio`
turns that condition off and leaves the others.

Move the window to your quietest hours. While a compaction runs, requests are answered as usual.
When it finishes, every session's cached view of the map is out of date at once. A background
pass rebuilds each one, and a session the pass has not reached yet rebuilds its own on its next
request. [Compaction](../system/write-path.md#compaction) describes the rest.

A compaction needs free disc space on the bundle's filesystem of one and a half times the bundle's
current files, and enough memory for its own estimate, which takes `MemoryMax` into account.
Without either it is refused, and `/control/status` records the reason under
`compaction.last_refusal`.

To start one yourself, whatever the schedule and the interval say:

```console
$ curl -sS -X POST --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  -w '%{http_code}\n' http://localhost/control/compact
202
```

The request is picked up at the next tick of the write side. A request made while a compaction is
running is dropped, with a warning in the log. Read the outcome a few seconds later:

```console
$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.cred)" \
  http://localhost/control/status | jq '.compaction | {folds, fold_refusals, last_refusal}'
{
  "folds": 1,
  "fold_refusals": 0,
  "last_refusal": null
}
```
