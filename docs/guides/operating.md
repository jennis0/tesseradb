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

