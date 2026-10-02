<!-- Generated from crates/tessera-config/src/lib.rs. Edit the doc comments there, then run: TESSERA_WRITE_CONFIG_REFERENCE=1 cargo test -p tessera-config config_reference -->

# tessera.toml

`tessera.toml` describes one deployment: where its bundle and working files are, which corpus declaration `tessera build` reads, and how `tessera serve` listens and bounds its work. `tessera build`, `tessera check`, `tessera health` and `tessera serve` read it from the working directory, or from the nearest directory above it that has one. `--deployment` names another file.

Every table refuses a key it does not know. A relative path is read from the directory this file is in, except the `unix:` socket path of `[serve] control`. No integer may be negative.

## `[bundle]`

Where the bundle and the server's own files are.

The table is required.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `path` | string (a path) | required | The bundle directory, which `tessera build` writes unless `--out` names another, and `tessera serve` opens. |
| `cache` | string (a path) | required | A directory outside the bundle for files the server derives: cached visibility masks, which survive a restart, and suggestion indexes and scratch files, which are rebuilt at each start. |
| `wal` | string (a path) | required | The write-ahead log. Every write through the control plane is appended and synced to disc here before it is acknowledged, and the log is replayed when the server starts. The path names a series of files: `wal.log` is written as `wal-000001.log`, `wal-000002.log` and so on, and is never itself a file. |

## `[build]`

What `tessera build` and `tessera check` read.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `schema` | string (a path) | `"schema.toml"` | The corpus declaration. `--config` names another. |

## `[plugin]`

The rule that reads credentials and access labels. It has one value.

The table is required.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `module` | string | required | `builtin:passthrough`, the one value, and any other is refused: a credential's terms are taken as presented, and every access label is an access expression. |

## `[disclosure]`

How long a viewer's token lasts.

The table is required.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `token_max_lifetime` | integer | required | The longest a session lasts, in seconds. A session ends sooner when the API key it was authorised with expires, or when the OIDC access token it was authorised with does. A token past its session's end is refused with 403. With `0`, a session has ended when it is issued. |

## `[serve]`

How `tessera serve` listens, whom it admits, and the limits on each request. `tessera build` reads none of it, and a file for building alone may leave the table out.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `viewer` | string | not set | The viewer plane's address and port, such as `"127.0.0.1:8141"`: `/v1/meta`, `/v1/viewport`, `/v1/items`, `/v1/artifacts` and `/v1/categories`, for requests carrying a viewer token. `tessera serve` refuses to start without it, and `tessera health` probes it. Port 0 takes a free port, which the server prints when it starts. |
| `session` | string | not set | The session plane's address and port: `POST /session/authorise`, where a principal holding `authorise-as` mints a session for another principal by API key, and `POST /session/revoke`. `tessera serve` refuses to start without it. |
| `control` | string | not set | The control plane's address and port, or `"unix:<path>"` for a Unix socket: ingest, deletion and suppression, declarations, flush, compaction and status, all under `/control`. `tessera serve` refuses to start without it. A relative socket path is read from the server's working directory, not from this file's directory, and a file already at the path is removed. |
| `operator_credential_file` | string (a path) | not set | A file holding the operator credential. It authenticates the built-in superuser, which holds every permission, `read-all` and `write-all` among them, and is not in the catalogue, so an empty catalogue still has an administrator. Its contents are trimmed. Changing the file and restarting rotates it. `tessera serve` refuses to start when the file cannot be read or holds only white space, and when neither this nor `operator_credential_env` is set. When both are set, the file is used. |
| `operator_credential_env` | string | not set | An environment variable holding the operator credential, trimmed as the file is. `tessera serve` refuses to start when it is unset or holds only white space. |
| `cors_origins` | array of strings | `[]` | Browser origins, such as `"https://maps.example.org"`, whose pages may call the viewer plane with a viewer token. `"*"` is refused. |
| `cors_loopback` | boolean | `false` | Admit a page served from `localhost`, `127.0.0.1` or `[::1]`, on any port, to the viewer plane, as for a notebook whose port is not known in advance. |
| `dev_cors_origins` | array of strings | `[]` | Browser origins whose pages may call both the viewer plane and the session plane, so a page in development can hold an API key that mints sessions for other principals. The server logs a warning at start when it is set. `"*"` is refused. |
| `compute_threads` | integer | the number of CPUs the process may use | Threads in the pool that computes responses. |
| `compute_admission` | integer | four per compute thread | Viewer and session requests computed at once. It admits `/v1/viewport`, the single-item and single-artifact reads, `/v1/artifacts/browse`, `/v1/aggregate`, a `/v1/categories/{column}/suggest` with `view` and `counts=true`, and `/session/authorise`, never the control plane. |
| `compute_queue` | integer | twice `compute_admission` | Requests that may wait for an admission slot beyond those running. A request finding no place is refused with 429 at once. `compute_admission` and `compute_queue` together may not exceed 2305843009213693951. |
| `admission_timeout_ms` | integer | `250` | Milliseconds a queued request waits for an admission slot before it is refused with 429. |
| `single_flight_wait_ms` | integer | `6000` | Milliseconds a request waits for another request's build of a shared cached structure before it is refused with 429. |
| `max_k` | integer | `1000` | The largest `k` a viewport request may name; a larger one is lowered to it. A tile draws at most the smaller of this and `k_max_marks`. |
| `k_min` | integer | `2` | The fewest marks a tile with a visible point draws. `tessera serve` refuses to start with `0`. |
| `k_max_marks` | integer | `500` | The most marks a tile draws, and the `k` of a request that names none. |
| `theta_target_marks` | integer | `16` | The marks the average occupied tile draws at any zoom; the threshold that samples points is derived from it. |
| `max_tiles_per_request` | integer | `262144` | The most tiles one `POST /v1/viewport` may cover. A request covering more is refused with 422. |
| `max_underlay_offset` | integer | `4` | The largest `underlay_offset` a viewport request may name: how many zoom levels below the tiles its exact masked counts are served at. A larger one is refused with 422, and `0` refuses every underlay. At most 255. |
| `max_underlay_cells` | integer | `8192` | The most underlay cells one viewport request may ask for, its tiles times 4 to the power of its `underlay_offset`. A request asking for more is refused with 422. |
| `stream_flush_bytes` | integer | `1048576` (1 MiB) | Bytes a streamed viewport response gathers before it sends a frame. A frame always ends at a whole tile. |
| `stream_write_stall_ms` | integer | `10000` | Milliseconds a streamed response waits for a client that has stopped reading before it cuts the response off. |
| `stream_deadline_ms` | integer | `60000` | Milliseconds a streamed response may run. A viewport response is cut off at it. A bulk read ends at it with a cursor to resume from. |
| `stage_timing` | boolean | `false` | Add each stage's timing, as `stage_ns`, to the last frame of a viewport response. It has an effect only in a binary built with the `bench-timing` feature. |
| `max_region_vertices` | integer | `10000` | The most vertices a `region` filter's polygon may have. A filter with more is refused with 422. |
| `max_region_cells` | integer | `262144` | The most boundary cells a `region` filter is resolved to at one zoom level. Past it the filter is answered for a cover of the polygon, which the `x-tessera-region` header reports, rather than refused. |
| `region_cache_bytes` | integer | `268435456` (256 MiB) | Bytes of resolved `region` filters kept for reuse, shared by every viewer. |
| `max_category_values` | integer | `1000` | The most values one page of `GET /v1/categories/{column}` returns, and the page size of a request that names none. A larger `limit` is lowered to it. |
| `max_suggestions` | integer | `20` | The most values one `/v1/categories/{column}/suggest` returns, `GET` or `POST`, and the `limit` of a request that names none. |
| `max_suggestion_walk` | integer | `100000` | The most values one suggestion request examines, hidden ones included, before it stops and answers `more: true`. |
| `max_suggest_set_entities` | integer | `10000000` | The size of a viewer's visible set at or below which suggestions are answered from a set of the values that viewer can see, built once per session, rather than by checking each value in turn. |
| `max_browse_rows` | integer | `200` | The most rows one page of `POST /v1/artifacts/browse` returns, and the page size of a request that names none. |
| `max_shape_vertices` | integer | `1000000` | The most vertices a shape published through `/control/layers/{name}/artifacts` may have. A shape with more is refused with 422. A build does not read this key: it refuses a shape of more than 1000000 vertices whatever the key says. |
| `max_page_rows` | integer | `100000` | The most rows one page of a bulk read, `POST /v1/items` or `POST /v1/artifacts`, holds. `0` is refused. |
| `max_page_bytes` | integer | `67108864` (64 MiB) | The most bytes one page of a bulk read holds, as Arrow before compression. A row larger than this is sent alone. `0` is refused, and so is a value above 2147483648. |
| `bulk_admission` | integer | `2` | Bulk reads running at once. One more is refused with 429 at once, and `0` refuses every bulk read. Bulk reads may hold seven times `max_page_bytes` of memory each. A value above 2305843009213693951 is refused. |
| `bulk_response_bytes` | integer | `268435456` (256 MiB) | The most bytes one bulk-read response carries before it ends with a cursor to resume from. A value below `max_page_bytes` is refused. |
| `bulk_response_ms` | integer | `30000` | Milliseconds one bulk-read response may run before it ends with a cursor to resume from. `stream_deadline_ms` ends one too, whichever comes first. |
| `max_aggregate_groupings` | integer | `16` | The most groupings one `POST /v1/aggregate` may ask for, each a table of the response. A request with more is refused with 422. |
| `max_aggregate_top` | integer | `1000` | The largest `top` one aggregate grouping may ask for. A larger one is refused with 422. |
| `max_aggregate_named` | integer | `1000` | The most values or artifacts one aggregate grouping may name. A longer list is refused with 422. |
| `max_aggregate_cells` | integer | `1048576` | The most cells one aggregate grouping's cell level may list: the cells at its depth in its area, however many groups share them. A request asking for more is refused with 422. |
| `aggregate_response_bytes` | integer | `16777216` (16 MiB) | The most bytes one `POST /v1/aggregate` response carries before it ends with a cursor to resume from. It runs under the viewport's admission, so this bounds what each one holds. A value below `aggregate_page_bytes` or above 2147483648 is refused. |
| `aggregate_page_bytes` | integer | `4194304` (4 MiB) | The most bytes one page of an aggregate response holds, as Arrow column bytes before compression. `0` is refused, and so is a value above 2147483648. |
| `visible_wait_max_secs` | integer | `30` | The most seconds a write asking to wait until it is visible, and `/control/flush`, wait before answering `visible: false`. |
| `row_projection_cache_bytes` | integer | `2147483648` (2 GiB) | Bytes of each viewer's projection of the rows they may see, kept between requests. |
| `fragment_cache_bytes` | integer | `1073741824` (1 GiB) | Bytes of cached visibility masks kept in memory. Masks also persist in the `[bundle]` cache directory, which this does not bound. |
| `masked_count_cache_bytes` | integer | `268435456` (256 MiB) | Bytes of per-viewer artifact counts kept for layers served from one bitmap per artifact. |
| `occupancy_cache_bytes` | integer | `33554432` (32 MiB) | Bytes of per-viewer counts of occupied tiles, from which the sampling threshold is derived. |
| `segment_floor_bytes` | integer | `16777216` (16 MiB) | Segments at or below this size are treated as one size tier when choosing which to merge. |
| `tier_width` | integer | `4` | How many segments of one size tier are merged together. `tessera serve` refuses to start with a value below 2. |
| `max_merged_segment_bytes` | integer | not set, and the server merges up to 268435456 bytes (256 MiB) | The largest segment a merge may produce, in bytes. |
| `coalesce_width` | integer | `8` | How many small files of one size tier, which flushes write for attribute values, records, text indexes and access terms, are combined into one. Unique index runs are combined four at a time whatever this says. `tessera serve` refuses to start with a value below 2. |

## `[ingest]`

Writes through the control plane: the limits on each request, and when buffered writes are published and compacted.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `ingest_admission` | integer | `64` | Requests to `/control/ingest` handled at once. One more is refused with 429 at once. A value above 2305843009213693951 is refused. |
| `ingest_queue_bound` | integer | `32` | Ingest batches that may wait for the writer. When the queue is full an ingest is refused with 429 and a `Retry-After`. Deletions and suppressions do not wait in it and are never refused for load. |
| `ingest_buffer_max_items` | integer | `1000000` | Ingested items that may wait for a flush. At or above it `/control/ingest` is refused with 429 before anything is written. |
| `ingest_max_batch_rows` | integer | `10000` | The most rows one `/control/ingest` request may carry. A request with more is refused with 422. |
| `ingest_max_batch_bytes` | integer | `16777216` (16 MiB) | The largest body `/control/ingest` accepts, in bytes. A larger one is refused with 422. |
| `commit_window_max_items` | integer | `10000` | Rows at which a commit window closes. Items allocated ids in one window are grouped by the access terms they carry, so a wider window stores the access index more compactly. `0` is read as 1. |
| `flush_max_age_secs` | integer | `90` | Seconds between flushes, which publish buffered writes as a new segment and make them visible. |
| `flush_max_items` | integer | `40000` | Buffered rows at which a flush runs before `flush_max_age_secs` has passed. |
| `publish_max_body_bytes` | integer | `67108864` (64 MiB) | The largest body `PUT` and `PATCH /control/layers/{name}/artifacts` accept, in bytes. A larger one is refused with 422. |
| `max_artifacts_per_request` | integer | `10000` | The most artifacts one `PUT /control/layers/{name}/artifacts` may publish. More is refused with 422. |
| `max_members_per_request` | integer | `5000000` | The most members one `PATCH /control/layers/{name}/artifacts` may add and remove, counted together. More is refused with 422. |
| `max_excluded_per_request` | integer | `1000000` | The most items one published artifact's `excluding` list may name. More is refused with 422. |
| `overlay_soft_limit` | integer | `500000` | Deletions and suppressions held in memory at which the server logs a warning and counts an alarm. Nothing is refused. |
| `compaction_min_interval_secs` | integer | `86400` | Seconds after one compaction starts during which the schedule starts no other. |
| `compaction_window_start` | string | `"00:00"` | When the daily compaction window opens, as `"HH:MM"` in UTC, or `"off"` for no window. Any other value is refused. |
| `compaction_window_secs` | integer | `14400` | How long the window stays open, in seconds. `0` never opens it, and a day or more never closes it. |
| `compaction_window_min_segments` | integer | `8` | Inside the window, compact when a view has at least this many segments. `0` never compacts in the window. |
| `compaction_max_segments` | integer or `"off"` | `64` | At any hour, compact when a view has this many segments, or `"off"`. |
| `compaction_after_deletions` | integer or `"off"` | the value of `overlay_soft_limit` | At any hour, compact when this many deleted items wait to be removed, or `"off"`. |
| `compaction_dead_rows_fraction` | number or `"off"` | `0.2` | At any hour, compact when deleted items waiting to be removed reach this fraction of the stored rows, or `"off"`. |
| `compaction_dead_bytes_ratio` | number or `"off"` | `1.0` | At any hour, compact when the bundle's unreferenced bytes on disc reach this ratio of the bytes its manifests name, or `"off"`. |

## `[catalogue]`

The identity catalogue: who may authenticate, with what, and the OIDC providers whose access tokens are accepted. `tessera build` reads none of it.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `dir` | string (a path) | not set | The directory holding the catalogue, a SQLite database of local principals, their password hashes and API keys, groups, grants and the providers declared through the API. It lives outside the bundle, so principals and grants carry across a rebuild. It is created, readable by the service's user alone, when absent. `tessera serve` refuses to start without it, or when another process holds it open. |
| `min_password_length` | integer | `15` | The fewest characters a password may hold when it is set. At least 1. |
| `failed_attempt_limit` | integer | `10` | Failed password attempts for one name within `failed_attempt_window` after which further attempts for that name are refused, answered as a wrong password is, until the oldest leaves the window. |
| `failed_attempt_window` | integer | `900` | The window, in seconds, over which failed password attempts are counted. |
| `providers` | array of tables | `[]` | OIDC providers the service starts with, each a table of `name`, `issuer`, `audience`, `jwks_url`, and optional `claim_rules` (each `{ claim, template }`) and `role_mappings` (each `{ claim, value, group }`). A provider declared here is listed by the API and cannot be changed or removed through it; edit this file and restart. The service refuses to start when a name is declared here and in the catalogue, or twice here. A `jwks_url` is `https`, or `http` to `localhost`, `127.0.0.1` or `[::1]`; the environment variable `TESSERA_ALLOW_INSECURE_JWKS=1` accepts any other `http` URL. |
