# Operate a deployment

This guide covers running Tessera once it is installed, [under systemd](systemd.md) or [with
Docker](docker.md). Before anyone else uses the server, decide who may reach each of its three
addresses and how much memory it gets. After that you'll mostly need the sections on health, the
write-ahead log and compaction. Putting the viewer address behind TLS has [a page of its
own](tls.md), and so does [rebuilding the bundle](rebuild.md).

The examples use the systemd install, run as the `tessera` user in `/srv/tessera`. Under Docker,
run them from `~/tessera-docker`, use ports 9161 to 9163 in place of 9151 to 9153, and write
`http://127.0.0.1:9163` where the examples reach the control socket with
`--unix-socket /run/tessera/control.sock http://localhost`.

## What the server keeps on disc

`[bundle]` in `tessera.toml` names three places.

| Key | What it holds | What makes it grow | If it is lost |
|---|---|---|---|
| `path` | The bundle: every item, its position, its attributes, the search indexes and the identity key. The server writes to it as it runs, adding a segment each time it makes newly ingested items visible and rewriting it at each compaction | Ingest, until a compaction reclaims what deletions and merges left behind | The database. Restore it from a backup |
| `wal` | The write-ahead log, which holds every change since the last flush into the bundle, in files named after this path: `wal-000001.log`, `wal-000001.sync` and so on | Writes, until the bundle holds them and the oldest files are deleted | Every change acknowledged since the last flush |
| `cache` | Work the server can redo: each distinct grant's set of visible items, each grant's counts for the annotation layers it has read, the name-search suggestions and scratch files | A pair of files for each distinct set of labels a viewer has been granted, and under `figures/` a file for each grant and annotation level read, up to `figures_disk_bytes`. A compaction deletes them | Nothing but time. The server rebuilds what it needs |

Back up the bundle and the log together, with the server stopped. A copy of one without the other
is a database as it stood at some other moment.

!!! warning
    Keep the bundle and the log on a local disc. The server locks the bundle's directory so that a
    second server cannot write to it, and that lock does not work reliably over NFS or SMB. Two
    servers writing one bundle overwrite each other's changes.

Anyone who can read the bundle can read every item in it, whatever its access label, and the
identity key too. Access control applies to what the server sends, not to the files.

## Addresses and credentials

| Address | Routes | Who should reach it |
|---|---|---|
| Viewer | `/v1/*`, `/healthz`, `/readyz` | Browsers, each with its own token, through a TLS proxy. People who log in with a password or an API key do it here, at `/v1/login`. |
| Session | `/session/authorise`, `/session/revoke`, `/healthz`, `/readyz` | Your application's backend, which signs people in and asks Tessera for their tokens with an API key whose principal holds `authorise-as` |
| Control | `/control/*` | You, and whatever loads data into the corpus |

An API key whose principal holds `authorise-as` can mint a token for any principal in the
catalogue, and so read whatever any principal may. The operator credential holds every permission:
it can mint a token for any set of access labels, read every item, change the catalogue, and
delete your data or add to it. Keep the session and control addresses off the internet. The viewer
address serves only what each token allows, and it is the one browsers need. It serves plain
HTTP and accepts a password over it, so put it behind TLS, as [Put Tessera behind TLS](tls.md)
describes.

Both installs keep the operator credential in a file called `operator.secret`. Under systemd it is
in `/srv/tessera/secrets`, and under Docker in `~/tessera-docker/secrets`, which Compose mounts into
the container as `/run/secrets`. The server reads the file once, when it starts, and trims the
whitespace around it. To change the credential, replace the file and restart the server. The users,
groups, API keys and grants are kept in the catalogue, the directory `[catalogue] dir` names, and
change without a restart.

Instead of a file, `operator_credential_env` can name an environment variable, which the server
trims in the same way. Anything else running as the same user can read a process's environment
from `/proc`, so a file is the better choice. Whichever you use, the server wants the credential
even if nothing ever calls the control address, and refuses one that is empty:

```text
tessera serve: refused to start: there is no operator credential; set `operator_credential_file` or `operator_credential_env` under [serve] and put the secret in that file or variable
```

A browser's token lasts `token_max_lifetime` seconds, set under `[disclosure]`, and keeps the
labels its principal held when it was issued. A change in the catalogue that could change them,
such as a label granted or taken away, a group membership or a disabled principal, ends every token
of the principals it affects. Their next request is answered `403 expired-token`, and the client
asks for a new token.

## Memory

The server holds these caches, each with a limit you can set under `[serve]`:

| Key | Default | What it holds |
|---|---|---|
| `row_projection_cache_bytes` | 2 GiB | For each session, which rows of the bundle it may see |
| `fragment_cache_bytes` | 1 GiB | For each distinct set of granted labels, the items it covers. Copies on disc in the cache directory outlive the memory |
| `masked_count_cache_bytes` | 1 GiB | Visible member counts, centroids and boxes for annotation levels served from a column, one entry per grant and level. Per artifact: 4 B for counts alone, 40 B with a centroid and a box, and for a layer serving a box 4 B more plus 128 B for each artifact with more than sixteen placed rows. A level of 1.4 million artifacts is 56 MB a grant counted with geometry, and a layer serving a box keeps at most 185 MB more of reserves in a cache of its own, so the default holds the counts of about nineteen such grant-levels and the reserves of at least five. Deployments where every user holds their own terms need one entry per user per level. An entry larger than the bound is served and not kept, which `masked_count_cache.not_admitted` on `/control/status` counts. Empty in a corpus without such a layer |
| `figures_disk_bytes` | 8 GiB | The same entries, and the labels of denied rows they are corrected with, kept in the cache directory so a restart reads them back rather than walking each grant's rows again. Size it as the memory bound above times the number of grant-levels a restart should find, plus about 24 B per denied row per level. Past it, the files least recently written or read are removed |
| `region_cache_bytes` | 256 MiB | Drawn filter regions broken into map tiles, shared between viewers |
| `occupancy_cache_bytes` | 32 MiB | How many tiles at each zoom hold something a session can see |

`masked_count_cache_bytes` bounds three caches, each to that size: the counts, the eight most
extreme rows on each side kept for an annotation layer that serves a box, and the tallies of number
and timestamp fields, which are under a kilobyte each. Beside them the server keeps what it
subtracts and adds on each request for deleted and suppressed rows and for recently ingested ones,
and the labels of denied rows, under fixed limits that come to 1.5 GiB.

The paged reads, `POST /v1/items` and `POST /v1/artifacts`, share one limit. Two may run at once
(`bulk_admission`), each holding up to seven pages of at most 64 MiB (`max_page_bytes`). The server
logs what that comes to when it starts, as `bulk_read_memory_bytes=939524096`. With the defaults,
the limits on the caches and the paged reads add up to about 8.7 GiB.

The bundle itself is not loaded. The server maps its files, so their pages sit in the kernel's page
cache and count towards the process's memory. When memory is short the kernel drops them, and reads
them back from disc when a request needs them. Set the cap (`MemoryMax` under systemd, or a memory
limit on the container) above the caches and paged reads together. Every gigabyte beyond that keeps
more of the bundle in memory. A cap that is too low makes the server slower before it makes it fail.

`/control/status` shows what the process holds. `anon_bytes` is the caches and the allocator's own
memory, and `file_bytes` is how much of the bundle is in memory:

```console
tessera$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.secret)" \
  http://localhost/control/status | jq '{posture: .write_executor.posture, heap}'
{
  "posture": "running",
  "heap": {
    "anon_bytes": 6320128,
    "file_bytes": 19673088,
    "last_trim_micros": 0,
    "last_trim_returned_bytes": 0,
    "resident_bytes": 25993216,
    "trim_baseline_bytes": 6139904,
    "trim_growth_bytes": 268435456,
    "trims": 0
  }
}
```

`compute_threads` defaults to the number of CPUs the process may use. Lower it to leave some for
other work on the same machine.

A build sizes itself separately. `tessera build --memory-budget 24g` keeps the build's own
structures within 24 GiB. Without it the build works out a budget from the memory the machine has
free, which is too much if the server is running beside it.

## Annotation layers and new viewers

Plan for one cost when viewers first open an annotation layer. Every level of a `flat`, `stacked` or
`tiered` layer whose memberships are enumerated is served from a column, one entry per row naming
its artifacts. The first time a viewer reads such a level under a grant the server has not seen,
whether by drawing it, colouring points by it or listing its artifacts, the server walks every row
the last build or compaction wrote that the grant may see, and counts each artifact's members, with
their positions where the layer serves a centroid or a box. Every later request under that grant
reads the result, corrected for whatever has been deleted, suppressed or ingested since.

The walk takes longer the more of the corpus the grant sees. On a bundle of the 3,495,729,729 GBIF
occurrences, with 12 threads, under a 24 GB memory cap and starting with nothing in the page cache,
one level of the taxonomy layer took:

| viewer sees | family | genus | species |
|---|---:|---:|---:|
| 1% | 0.29 s | 0.24 s | 1.1 s |
| 25% | 3.6 s | 2.2 s | 3.2 s |
| everything | 8.8 s | 10.7 s | 12.5 s |

*One run each, from `probes/2026-10-06-first-open-fills/`. The layer serves a centroid and a box.*

So a viewer who sees 1% of that corpus waits between a fifth of a second and a second for each level
the first time, and one who sees everything waits 9 to 12 seconds. A grant is a set of labels, and
every viewer holding the same set shares one result. A deployment whose viewers fall into a few
groups pays the walk a few times for each level. A deployment in which every user holds labels of
their own, such as a label per document, pays it for each user and each level they open.

The result is written to the cache directory, so a restart reads it back rather than walking
again. A compaction changes the bundle's identity, and each grant walks again the next time it
reads each level. `figures_disk_bytes` decides how many results the directory keeps, and
`masked_count_cache_bytes` how many stay in memory. `/control/status` counts walks under
`masked_count_cache.fills` and results read back from disc under `masked_count_cache.loads`.

The artifacts a map draws come from `POST /v1/artifacts/viewport`, which has an admission limit of
its own, `artifact_admission`, one request per compute thread by default. A request past it, and
past as many waiting, is answered 429, and `/control/status` counts those under
`artifacts.shed_total`. `max_artifacts_per_tile`, 1,000 by default, is the most artifacts one
request may ask for in each tile and level.

## Health and readiness

The viewer and session addresses both answer `/healthz` and `/readyz`, with no credential and no
body. `/healthz` answers 200 whenever the process is answering at all. `/readyz` is the one to send
traffic by. It answers 200 while the part of the server that takes writes is running normally, and
503 when it is not, or when the server has had to open an older state of the bundle because the
newest one on disc failed its checks. The control address has neither.

The [TLS proxy](tls.md) does not pass the probes through. Run your load balancer's or monitor's
check on the server's machine, or against the session address from inside your network.
`tessera health` makes the same check from the command line. It reads `tessera.toml`, asks the
viewer address for `/readyz`, and exits 0 on a 200. The Docker image uses it as its health check.

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

## The write-ahead log

The server writes every change to the log, and forces it to disc, before it tells the caller the
change succeeded. A change it has acknowledged therefore survives the server being stopped or
killed.

### When a write to the log fails

If appending the record fails, the posture becomes `wal-poisoned` and `/readyz` answers 503 until
the server restarts.

If the record was appended but forcing it to disc fails, a deletion or suppression is tried again,
twice. Ingest is not. When the write still fails, the server throws away what did not reach the
disc and goes back to `running`, and `/readyz` answers 200 again. The only sign in
`/control/status` is `write_executor.wal_recoveries` going up by one. The server is not back to
normal, though. It makes no newly ingested items visible and never trims the log. It starts no
compaction, not even one you ask for, and records nothing about that under `compaction`. The log
has a line beginning `ALARM` and ending "Restart this node."

Either way, a deletion or suppression whose write failed is still applied in memory, so the item
stays hidden. The caller is answered 500, with a detail that includes "a change in it may be in
force, so do not treat this as a no-op". An unsuppress whose write failed is not applied. Nothing on
disc records the deletions and suppressions that were applied, so a restart forgets them.

!!! warning
    A restart can bring a deleted or suppressed item back into view if the change that hid it was
    answered 500. After a failed write:

    1. Have whatever sends changes to `/control/changes` send again every change that was answered
       500. If the log has recovered, this writes them to disc.
    2. Fix the disc.
    3. Restart the server.
    4. Send every change that was answered 500 once more. This catches any that failed again in
       step 1.

    Restart the server when its process exits, and never because `/readyz` answers 503. The
    systemd unit and the Compose file in these guides both work that way.

## Compaction

The server adds a segment to the bundle each time it makes newly ingested items visible, and merges
small segments as they build up. Deleted items stay in the bundle, hidden from every viewer, until a
compaction rewrites the bundle as one segment per view without them. Compaction also reclaims the
disc space that merges leave behind.

The server starts one when any of these conditions holds, each set under `[ingest]`:

| Key | Default | Starts a compaction when |
|---|---|---|
| `compaction_window_start`, `compaction_window_secs`, `compaction_window_min_segments` | `"00:00"`, 14400, 8 | A view has 8 segments or more during the four hours from midnight UTC |
| `compaction_max_segments` | 64 | A view has 64 segments, at any hour |
| `compaction_after_deletions` | 500000, the value of `overlay_soft_limit` | 500,000 deleted items are waiting to be removed |
| `compaction_dead_rows_fraction` | 0.2 | Deleted items waiting to be removed are at least a fifth of all rows |
| `compaction_dead_bytes_ratio` | 1.0 | Files the bundle no longer uses take at least as much space as the files it does |

Set `compaction_window_start` to the start of your quietest hours, in UTC as `"HH:MM"`. Requests are
answered as usual while a compaction runs. When it finishes, though, every session's cached view of
the map is out of date at once. A background pass rebuilds them, and a session it hasn't reached
yet pays for its own on its next request.

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
  -H "authorization: Bearer $(cat secrets/operator.secret)" \
  -w '%{http_code}\n' http://localhost/control/compact
202
```

The server picks the request up at its next tick. A request made while a compaction is running is
dropped, with a warning in the log. Read the outcome a few seconds later:

```console
tessera$ curl -sS --unix-socket /run/tessera/control.sock \
  -H "authorization: Bearer $(cat secrets/operator.secret)" \
  http://localhost/control/status | jq '.compaction | {folds, fold_refusals, last_refusal}'
{
  "folds": 1,
  "fold_refusals": 0,
  "last_refusal": null
}
```

Between compactions, a write can stall the write side in one case. When a growth or a publication
gives an item a second artifact in a level the server answers from one label per row, and the
server holds that level in memory, it rewrites the level's column as a list, and its member file
with it, on the thread that applies writes. Other writes wait until it finishes. The member file
alone takes about two and a half minutes a level at the scale of a 3.5-billion-row corpus. A level
the server does not hold is composed as a list by the next request that reads it. The next
compaction records the level as a list and writes both files again.

[Compaction](../system/write-path.md#compaction) in the write-path chapter describes it in full.

## Stopping and restarting

SIGTERM or SIGINT stops the server at once, without waiting for requests in progress, and it
writes `tessera serve: stopped on SIGTERM` as it goes. A browser partway through a response gets it
cut short, missing the final frame that marks a response complete.

Sessions are held only in memory. After a restart none of them exists, and each browser has to ask
your backend for a new token. Changes are a different matter. On starting, the server opens the
newest state of the bundle on disc that passes its checks and replays the log over it, which brings
back every change it acknowledged before it stopped. [Restart
and recovery](../system/write-path.md#restart-and-recovery) has the detail.
