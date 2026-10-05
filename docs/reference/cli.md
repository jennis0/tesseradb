<!-- Generated from crates/tessera-cli/src/main.rs. Edit the help text there, then run: TESSERA_WRITE_CLI_REFERENCE=1 cargo test -p tessera-cli cli_reference -->

# CLI

`tessera <subcommand> --help` prints the text on this page. `tessera --version` prints the commit the binary was built from. It prints `unknown` when `TESSERA_BUILD_COMMIT` was unset at build time and git could not name the commit, as in a build outside a git checkout.

`build`, `check`, `health` and `serve` read the deployment file `tessera.toml` from the working directory, or from the nearest directory above it that has one. `--deployment` names a different file.

## `tessera build`

```text
tessera build [OPTIONS]
```

Build a bundle from the corpus declaration and the source files it names.

`tessera build` needs no flags. It reads `tessera.toml` from the working directory, or from the nearest directory above it that has one. That file names the corpus declaration in `[build] schema` (default `schema.toml`) and the bundle directory in `[bundle] path`, each relative to the file's own directory. The declaration names the source files.

Every build creates a new bundle with a new key for its `tessera_id`s, so a `tessera_id` read from an earlier bundle does not name an item in this one. A copy of a bundle keeps its `tessera_id`s.

The other flags override `tessera.toml` or tune the build.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
| `--out` | `PATH` |  | Write the bundle to this directory instead of `[bundle] path` in `tessera.toml`. `tessera serve` opens `[bundle] path`, so it does not serve a bundle written elsewhere. |
| `--limit` | `VALUE` |  | Build only the items whose value of the declaration's one unique integer attribute is below this value, and the rows of every file that name them. A view's points row whose value is at or above the limit, or null, creates no item. A negative value counts as its unsigned 64-bit value, at least 2^63, so the limit drops it. Refused when the declaration has no unique integer attribute, or more than one. A row that names a kept item by any unique field is read whatever its own value. A row that names none is left out: in a view's points it creates no item where its value is at or above the limit or null, and in any other file it is reported as outside the limit, since it may name an item the limit left out. The exception is a file that names items by the limit's attribute alone, whose row naming no item is refused where its value is below the limit. |
| `--strict` |  |  | Refuse the build at the first file with a row the identity rule refuses. Without it, a row naming two items, naming an item or a unique value an earlier row of its file names, or, outside a view's points, naming no item, is left out and the build goes on. The refused rows are printed, and written to `reports/refused.json` in the bundle where there are any. |
| `--config` | `PATH` |  | Read this corpus declaration instead of `[build] schema` in `tessera.toml`. The declaration is compiled into the bundle's `MANIFEST.json`, and the server reads it from there. |
| `--file` | `NAME=PATH` |  | Read the source NAME in the declaration's `[sources]` from PATH instead. Repeatable. A relative PATH is read from the working directory, not from the declaration's directory. Every block that reads the source reads PATH. Refused when `[sources]` has no source called NAME (the message lists the names it has), and when NAME is given twice. The flag replaces a source's path and cannot add a source. |
| `--no-oracle-pairs` |  |  | Do not write `pairs.parquet`. The server does not read the file. The conformance suite and `tessera verify --deep` do, and `verify --deep` passes a bundle without one. |
| `--batch-items` | `ITEMS` | derived | Assign internal ids in batches of this many items. Default: the largest batch the memory budget allows, which is the whole corpus when it fits. Refused when the batch does not fit the budget, or when it is less than half the size the budget allows. A build of more than one batch records the size in the bundle. |
| `--memory-budget` | `SIZE` | derived | Peak memory for the build's own structures, in bytes or with a `k`, `m` or `g` suffix, such as `24g`. Default: 80% of the available memory or of the process's cgroup limit, whichever is lower, kept between 2 GiB and 1 TiB, and 24 GiB where available memory cannot be read. Batch and band sizes follow from it. A build that would not fit is refused before it assigns internal ids, with the arithmetic in the message. |
| `--stage-timings` |  |  | Print each build stage's wall time, row count and peak resident memory to stderr as the stage ends. Peak resident memory is the process's high-water mark when the stage ended, so it shows the stage in which the peak was reached. |
| `--stage-timings-json` | `PATH` |  | Write the per-stage records to this file as JSON when the build ends, including one that fails partway. One object per stage, in order, with `stage`, `wall_s`, `rows`, `peak_rss_kib`, `started_at` and `ended_at`. It does not need `--stage-timings`. |

## `tessera check`

```text
tessera check [OPTIONS]
```

Check the declaration against the column schemas of the files it names, without reading a row.

`tessera check` finds `tessera.toml`, the declaration and the `--file` overrides as `tessera build` does, and stops at the first error in `tessera.toml` or the declaration, such as a missing key or a TOML syntax error. Once both parse, it reads the footer of each source Parquet file and reports every column that is missing or has the wrong type, not only the first. It reads no rows except the geometry of shape layers, which it reads to size them.

The report goes to stderr: the files read, the findings, warnings, the columns each file names items by, the frames the views will have, the view groups, the shape layers' sizes, and on a clean check the disclosure table. The exit status is non-zero when there is a finding, such as a file other than a view's points with no column to name items by; a warning does not change it.

It cannot check anything that needs a row: whether a closed vocabulary covers the values in the data, which rows the identity rule refuses, or where the data lies in its view's extent. `tessera build` reports those.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
| `--config` | `PATH` |  | Read this corpus declaration instead of `[build] schema` in `tessera.toml`. |
| `--file` | `NAME=PATH` |  | Read the source NAME in the declaration's `[sources]` from PATH instead. Repeatable, and the same override `tessera build` takes. |
| `--payloads` |  |  | Print the declaration to stdout as the JSON request bodies the control plane takes, to declare the same corpus on a running service. Printed only when the check has no finding. The output is one object with the keys `layers`, `attributes`, `vocabularies`, `views` and `view_groups`, each an array in declaration order. A layer or attribute entry is the request body itself. A vocabulary, view or view group entry is `{ "name", "body" }`, because its name goes in the route's path. A vocabulary entry also has `values`, the body for `PATCH /control/vocabularies/{name}/values`, or `values_source`, the `[sources]` name its values are read from. Values read from a source are not printed. |

## `tessera verify`

```text
tessera verify [OPTIONS] <BUNDLE>
```

Verify a bundle's files and every row's `tessera_id`.

It opens the bundle as the server does. That checks every manifest digest, the size and SHA-256 of every file except the unique indexes' runs, and that each segment's permutation maps one-to-one onto its rows. `--deep` hashes those runs too. It then confirms that the row space holds exactly the rows the segments claim, and computes each row's `tessera_id` again from the identity key, failing on the first row that differs.

It also prints to stderr each indexed keyword column's count of distinct values against its rows, with a warning for a column whose values are unique per row. A warning does not fail the verify.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `BUNDLE` |  |  | The bundle directory, the one holding `CURRENT`. |
| `--deep` |  |  | Also check the bundle's internal structures. The term lists must be sorted, free of duplicates and in range; dictionary records must not repeat; record blobs, Morton cells, cell codes and identity bands must agree with the columns they index or copy, as must each level's band label copy; each group-scoped render column must be present in every segment; each unique column's index must be hashed against the manifest, name at most one live item for a value and agree with the column's values in both directions; and `pairs.parquet`, when present, must match the term lists it was written with. Run it on a bundle no running server is writing to. |

## `tessera tokenise`

```text
tessera tokenise [OPTIONS]
```

Split text into tokens with an analyser built into this binary. Each input line prints as one line, its tokens separated by tabs.

When a `match` filter returns nothing, this shows how the index split the text. `/v1/meta` publishes each text column's analyser identity as `declared_scalars[].analyser`, such as `unicode/icu4x-2.2/p1`. The analyser's name is the part before the first `/`; run the query text through the analyser of that name. The conformance suite uses the command to compute expected `match` results with the analyser the index was built with.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--text` | `TEXT` |  | Text to split. Repeatable. With none, reads one input per line from stdin. |
| `--analyser` | `NAME` | `unicode` | The analyser to use, by name. An unknown name is refused with the list of names this binary carries. |
| `--identity` |  |  | Print the analyser's identity, the value a text column records, and exit. |

## `tessera health`

```text
tessera health [OPTIONS]
```

Ask a running server whether it is ready, and exit 0 if it is or 1 if it is not.

Finds `tessera.toml` as `tessera build` does and sends `GET /readyz` to the viewer address in `[serve]`. The server answers 503 while its write-ahead log cannot be written, when the thread that applies writes has stopped, and when part of the bundle is served from an older list of segments because the newest could not be used. Otherwise it answers 200, even while a write to disc hangs.

The command exits 0 on a 200. It exits 1, with the reason on stderr, on any other answer, on no answer within `--timeout`, when nothing is listening, and when `tessera.toml` is refused or declares no viewer address. A server starts listening only once it has opened its bundle, so the check fails until then.

Exit 1 means stop sending viewer requests to the server, and keep sending it deletions and suppressions, which it still applies when its log has failed. It does not call for a restart: a restart undoes each deletion or suppression the server answered with 500 because the log could not be written, until that change is sent again.

A viewer address of `0.0.0.0` or `[::]` is reached on loopback, so run the command on the machine or in the container the server runs in. The Docker image's health check runs it.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
| `--timeout` | `SECONDS` | `3` | Seconds the whole request may take, from connecting to reading the answer. |

## `tessera items`

```text
tessera items [OPTIONS] --view <VIEW> --fields <NAMES> --server <URL>
```

Read every item a session token may see in one view, with the fields named, from a running server, and write them as Arrow IPC or Parquet.

The server answers `POST /v1/items` a page at a time, several pages to a response, and ends each response with a cursor for the next. This requests responses until no row remains and writes each page as it arrives. Each of the route's fields is the argument of the same name, sent only when given, so the server's own setting applies otherwise.

The columns are `tessera_id`, the fields in the order named, the system fields in the order named, then `tessera:matched` under `--keep-unmatched`. A category field is a dictionary column of its value keys, each page's dictionary holding the keys of its own rows. A read that returns no row writes these columns with no rows.

A read cut short leaves the whole pages read before it in the output, exits 1 and prints the cursor to read the rest with. The first response's head, with the counts under `--count`, is printed on stderr at the end.

For example, `tessera items --server http://127.0.0.1:8080 --view papers --fields title,year --system-fields labels --out papers.parquet`.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--view` | `VIEW` |  | The view to read, as `/v1/meta` names it. An item with no position in it is not returned. |
| `--fields` | `NAMES` |  | The declared fields to return, comma-separated, in the order wanted. `--fields ''` returns `tessera_id` alone. A field declared for a view group, read under a view outside that group, is named `<field>@<key>`. An undeclared or repeated field is refused. |
| `--system-fields` | `NAMES` |  | Any of `position` and `labels`, comma-separated, in the order wanted: the columns `tessera:x` and `tessera:y`, and `tessera:labels`. Any other name is refused. |
| `--filters` | `JSON` |  | A filter expression as JSON, such as `{"year": {"range": {"gte": 2020}}}`. Only the items that match are returned. |
| `--keep-unmatched` |  |  | Return every item, with a `tessera:matched` column saying whether it matches `--filters`. |
| `--count` |  |  | Count the items the token may see in the view and those that match. The counts are printed on stderr at the end. Refused with `--cursor`. |
| `--order` | `ORDER` |  | `map` returns the items by their place on the map; `stored` in the order the server stores records, which is faster for a field that is neither rendered nor indexed. Without it the order is the one `--cursor` was read in, or the server's choice. With `--cursor`, another order is refused. |
| `--page-rows` | `N` |  | Rows in a page, at most the server's `selection.max_page_rows`, which applies without it. 0 is refused. |
| `--pages` | `N` |  | The most pages in one response. Responses are requested until the read is done. 0 is refused. |
| `--cursor` | `CURSOR` |  | Start after the last page of an earlier read: the cursor a read cut short printed. |
| `--compression` | `COMPRESSION` |  | `zstd` compresses the pages on their way from the server. The output is written uncompressed either way. |
| `--server` | `URL` |  | The viewer plane's address, such as `http://127.0.0.1:8080`. |
| `--token` | `TOKEN` |  | A session token, as `tessera login` or `tessera session authorise` prints one. Without it the token is read from `TESSERA_TOKEN`, and with neither the read is refused. |
| `--out` | `PATH` |  | The file to write. Without it, or with `-`, the output goes to stdout. |
| `--format` | `FORMAT` |  | `ipc` writes an Arrow IPC stream and `parquet` a Parquet file. Without it the format is the extension of `--out`, `.arrows` or `.parquet`; stdout and any other extension need it. A format that disagrees with the extension of `--out` is refused. |

## `tessera artifacts`

```text
tessera artifacts [OPTIONS] --view <VIEW> --layer <LAYER> --fields <NAMES> --server <URL>
```

Read every artifact of one layer a session token is served, with the properties named, from a running server, and write them as Arrow IPC or Parquet.

An artifact is one member of a layer: a cluster, a region, a node in a taxonomy. The read is carried across `POST /v1/artifacts` responses, and written, as `tessera items` carries and writes one. The columns are `tessera_id`, the properties in the order named, then `matched_count` under `--filters`. The rows are in order of level, and in the order they were published within a level.

For example, `tessera artifacts --server http://127.0.0.1:8080 --view papers --layer clusters --fields key,masked_count --format ipc > clusters.arrows`.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--view` | `VIEW` |  | The view whose items the counts and the geometry are computed over. |
| `--layer` | `LAYER` |  | The layer to read, as `/v1/meta` lists it. A layer not published to the token in `--view` is refused. |
| `--fields` | `NAMES` |  | Any of `key`, `level`, `parents`, `target`, `masked_count`, `content`, `centroid`, `box` and `shape`, comma-separated, in the order wanted. Any other name, or one repeated, is refused. |
| `--level` | `LEVEL` |  | Only the artifacts at this level of a levelled layer. Refused on a layer with one level, and past the levels the layer holds. |
| `--parent` | `TESSERA_ID` |  | Only the children of this artifact, by its `tessera_id`. Refused with `--q`. |
| `--q` | `TEXT` |  | Only the artifacts whose key, or first text, contains this, ignoring case. Refused with `--parent`. |
| `--filters` | `JSON` |  | A filter expression as JSON. Only the artifacts with a visible member that matches are returned, each with a `matched_count` column. |
| `--keep-unmatched` |  |  | With `--filters`, return every artifact, those with no matching member included. |
| `--count` |  |  | Count the artifacts served and those that match. The counts are printed on stderr at the end. Refused with `--cursor`. |
| `--page-rows` | `N` |  | Rows in a page, at most the server's `selection.max_page_rows`, which applies without it. 0 is refused. |
| `--pages` | `N` |  | The most pages in one response. Responses are requested until the read is done. 0 is refused. |
| `--cursor` | `CURSOR` |  | Start after the last page of an earlier read: the cursor a read cut short printed. |
| `--compression` | `COMPRESSION` |  | `zstd` compresses the pages on their way from the server. The output is written uncompressed either way. |
| `--server` | `URL` |  | The viewer plane's address, such as `http://127.0.0.1:8080`. |
| `--token` | `TOKEN` |  | A session token, as `tessera login` or `tessera session authorise` prints one. Without it the token is read from `TESSERA_TOKEN`, and with neither the read is refused. |
| `--out` | `PATH` |  | The file to write. Without it, or with `-`, the output goes to stdout. |
| `--format` | `FORMAT` |  | `ipc` writes an Arrow IPC stream and `parquet` a Parquet file. Without it the format is the extension of `--out`, `.arrows` or `.parquet`; stdout and any other extension need it. A format that disagrees with the extension of `--out` is refused. |

## `tessera aggregate`

```text
tessera aggregate [OPTIONS] --view <VIEW> --grouping <JSON> --server <URL>
```

Count how the items a session token may see in one view are distributed, by one grouping, from a running server, and write the table as Arrow IPC or Parquet.

The grouping is `POST /v1/aggregate`'s, as JSON: `{}` is the size of the set; `{"by": {"field": "venue", "top": 10}}` the ten commonest values of a category field; `{"by": {"field": "year", "bins": 20}}` a histogram of a number or timestamp field; `{"by": {"layer": "clusters", "top": 10}}` the largest artifacts of a layer; `{"cells": {"depth": 8}}` a density surface. The read is carried across responses, and written, as `tessera items` carries and writes one. The table's head, with the set's total, is printed on stderr at the end.

For example, `tessera aggregate --server http://127.0.0.1:8080 --view papers --grouping '{"by": {"field": "submitted_at", "bins": 12}}' --format ipc > submitted.arrows`.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--view` | `VIEW` |  | The view the counts are taken in. An item with no position in it is not counted. |
| `--grouping` | `JSON` |  | One grouping as JSON, such as `{"by": {"field": "year", "bins": 20}}`. `{}` is the size of the set. |
| `--filters` | `JSON` |  | A filter expression as JSON. Only the items that match are counted. |
| `--reference` | `JSON` |  | A second set to compare each count with, as a filter expression in JSON; `{}` is every item the token may see in the view. The table then has `reference_count` and `lift`. |
| `--page-rows` | `N` |  | Rows in a page, at most the server's `selection.max_page_rows`, which applies without it. 0 is refused. |
| `--pages` | `N` |  | The most pages in one response. Responses are requested until the read is done. 0 is refused. |
| `--cursor` | `CURSOR` |  | Start after the last page of an earlier read: the cursor a read cut short printed. |
| `--compression` | `COMPRESSION` |  | `zstd` compresses the pages on their way from the server. The output is written uncompressed either way. |
| `--server` | `URL` |  | The viewer plane's address, such as `http://127.0.0.1:8080`. |
| `--token` | `TOKEN` |  | A session token, as `tessera login` or `tessera session authorise` prints one. Without it the token is read from `TESSERA_TOKEN`, and with neither the read is refused. |
| `--out` | `PATH` |  | The file to write. Without it, or with `-`, the output goes to stdout. |
| `--format` | `FORMAT` |  | `ipc` writes an Arrow IPC stream and `parquet` a Parquet file. Without it the format is the extension of `--out`, `.arrows` or `.parquet`; stdout and any other extension need it. A format that disagrees with the extension of `--out` is refused. |

## `tessera serve`

```text
tessera serve [OPTIONS]
```

Serve the bundle that `tessera.toml` names.

Finds `tessera.toml` as `tessera build` does, opens the bundle at `[bundle] path` and replays the write-ahead log at `[bundle] wal`, all before it binds any address. It then listens on the viewer, session and control addresses in `[serve]`. When all three are bound it prints one line of JSON to stdout naming them. Diagnostics go to stderr, in colour only when stderr is a terminal.

SIGTERM or SIGINT stops the server at once, even while it opens the bundle, and it exits 0. Stopping ends every viewer session, because sessions are held in memory. A write is acknowledged only once the write-ahead log holds it on disc, so stopping loses no acknowledged write. A deletion or suppression answered with 500 because the log could not be written is in force but not on disc, and a restart undoes it until the change is sent again.

It refuses to start, and exits 1, when `tessera.toml` is refused as `tessera build` would refuse it or lacks one of the three `[serve]` addresses. It refuses when the operator credential is not set, is empty, or its file cannot be read: under `[serve]`, `operator_credential_file` or `operator_credential_env` names the file or environment variable holding it. It refuses when `[catalogue] dir` is not set, or the catalogue there cannot be opened or is held open by another process. It also refuses when the bundle cannot be read and when the write-ahead log fails its checksum. An address that cannot be bound, such as one already in use, stops it with exit 1 after the bundle has opened.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |

## `tessera login`

```text
tessera login --server <URL> <--principal <NAME>|--api-key|--access-token>
```

Log in on the viewer plane and print the session token and its `expires_at` as JSON.

The password, API key or OIDC access token is read from the first line of stdin, never from an argument. The principal must hold `read`.

For example, `tessera login --server http://127.0.0.1:8080 --principal ann < password.txt`.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--server` | `URL` |  | The viewer plane's address, such as `http://127.0.0.1:8080`. |
| `--principal` | `NAME` |  | Log in as this local principal, with the password read from the first line of stdin. |
| `--api-key` |  |  | Log in with the API key read from the first line of stdin. |
| `--access-token` |  |  | Log in with the OIDC access token read from the first line of stdin. |

## `tessera logout`

```text
tessera logout --server <URL>
```

End a session on the viewer plane. The session token is read from `TESSERA_TOKEN`, never from an argument.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--server` | `URL` |  | The viewer plane's address, such as `http://127.0.0.1:8080`. The session token to end is read from `TESSERA_TOKEN`. |

## `tessera session`

```text
tessera session <COMMAND>
```

Mint and revoke sessions on the session plane, and list and end sessions on the control plane.

The session plane's verbs read their credential from `TESSERA_API_KEY`: an API key holding `authorise-as`, or the operator credential, which alone may name the session's terms or ask for a session reading every item. The control plane's read their credential from `TESSERA_CREDENTIAL` and need `admin`.


## `tessera session authorise`

```text
tessera session authorise --session <URL> <--principal <NAME>|--access-token|--term <TERM>|--read-all>
```

Mint a session on the session plane, with the credential in `TESSERA_API_KEY`: an API key whose principal holds `authorise-as`, or the operator credential.

A session for a principal carries the target's terms and its `read` and `write`, and never its `read-all` or `write-all`. With the operator credential alone, a session for `--term`s holds those terms and `read`, and a session for `--read-all` reads every item. It prints `token`, `token_id` and `expires_at` as JSON.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--session` | `URL` |  | The session plane's address, such as `http://127.0.0.1:8081`. |
| `--principal` | `NAME` |  | Act as this local principal. |
| `--access-token` |  |  | Act as the OIDC identity whose access token is read from the first line of stdin. |
| `--term` | `TERM` |  | A term the session holds, with the operator credential. Repeatable. |
| `--read-all` |  |  | A session of the superuser itself, which reads every item, with the operator credential. |

## `tessera session revoke`

```text
tessera session revoke --session <URL> --token-id <N>
```

End a session by its `token_id` on the session plane, with the credential in `TESSERA_API_KEY`: an API key ends a session minted with a key of the same principal, and the operator credential ends any session.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--session` | `URL` |  | The session plane's address, such as `http://127.0.0.1:8081`. |
| `--token-id` | `N` |  | The `token_id` `tessera session authorise` printed. |

## `tessera session list`

```text
tessera session list [OPTIONS] --control <ADDRESS>
```

List live sessions on the control plane: all of them, or a local principal's or a provider's.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `--principal` | `NAME` |  | Only this local principal's sessions, including those minted for it. |
| `--provider` | `NAME` |  | Only the sessions authorised through this provider. |

## `tessera session end`

```text
tessera session end --control <ADDRESS> <--token-id <N>|--principal <NAME>|--provider <NAME>>
```

End one session by `token_id`, or every session of a local principal or a provider, on the control plane.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `--token-id` | `N` |  | The one session with this `token_id`. |
| `--principal` | `NAME` |  | Every session of this local principal, including those minted for it. |
| `--provider` | `NAME` |  | Every session authorised through this provider. |

## `tessera principal`

```text
tessera principal <COMMAND>
```

Manage local principals on the control plane: people and services.

Every verb reads the control plane's credential from `TESSERA_CREDENTIAL`, which is the operator credential, an API key or an OIDC access token, and needs `admin`. Each prints the server's JSON answer; a change answers how many sessions it ended.


## `tessera principal list`

```text
tessera principal list --control <ADDRESS>
```

List every local principal.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |

## `tessera principal show`

```text
tessera principal show --control <ADDRESS> <NAME>
```

Show one principal: its kind, flags, terms, permissions and groups.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera principal create`

```text
tessera principal create --control <ADDRESS> --kind <KIND> <NAME>
```

Create a local principal, holding nothing.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |
| `--kind` | `KIND` |  | `person` or `service`. |

## `tessera principal delete`

```text
tessera principal delete --control <ADDRESS> <NAME>
```

Delete a principal with its password, API keys, grants and memberships.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera principal disable`

```text
tessera principal disable --control <ADDRESS> <NAME>
```

Disable a principal. Its sessions end, and none of its credentials is accepted.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera principal enable`

```text
tessera principal enable --control <ADDRESS> <NAME>
```

Enable a disabled principal.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera principal set-password`

```text
tessera principal set-password --control <ADDRESS> <NAME>
```

Set a principal's password, read from the first line of stdin.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera principal clear-password`

```text
tessera principal clear-password --control <ADDRESS> <NAME>
```

Remove a principal's password.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The principal's name. |

## `tessera key`

```text
tessera key <COMMAND>
```

Manage API keys on the control plane. The credential is read as `tessera principal` reads it.


## `tessera key list`

```text
tessera key list --control <ADDRESS> <PRINCIPAL>
```

List a principal's API keys, by prefix, without their secrets.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `PRINCIPAL` |  |  | The principal's name. |

## `tessera key create`

```text
tessera key create [OPTIONS] --control <ADDRESS> <PRINCIPAL>
```

Issue an API key for a principal. The whole key is printed once, here.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `PRINCIPAL` |  |  | The principal's name. |
| `--expires-at` | `SECONDS` |  | When the key, and every session authorised with it, ends, in seconds since the Unix epoch. Without it the key does not expire. |
| `--permission` | `NAME` |  | A permission the key holds: `read`, `write`, `authorise-as`, `admin`, `read-all` or `write-all`. Repeatable. Without it the key holds its principal's permissions. |

## `tessera key revoke`

```text
tessera key revoke --control <ADDRESS> <PREFIX>
```

Revoke an API key by its prefix. Every session authorised or minted with it ends.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `PREFIX` |  |  | The key's prefix, as `tessera key list` prints it. |

## `tessera group`

```text
tessera group <COMMAND>
```

Manage local groups and their members on the control plane. The credential is read as `tessera principal` reads it.


## `tessera group list`

```text
tessera group list --control <ADDRESS>
```

List every local group.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |

## `tessera group show`

```text
tessera group show --control <ADDRESS> <NAME>
```

Show one group: its terms, permissions and members.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The group's name. |

## `tessera group create`

```text
tessera group create --control <ADDRESS> <NAME>
```

Create a local group.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The group's name. |

## `tessera group delete`

```text
tessera group delete --control <ADDRESS> <NAME>
```

Delete a group with its grants and memberships.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The group's name. |

## `tessera group add-member`

```text
tessera group add-member --control <ADDRESS> <GROUP> <PRINCIPAL>
```

Add a principal to a group.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `GROUP` |  |  | The group's name. |
| `PRINCIPAL` |  |  | The principal's name. |

## `tessera group remove-member`

```text
tessera group remove-member --control <ADDRESS> <GROUP> <PRINCIPAL>
```

Remove a principal from a group.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `GROUP` |  |  | The group's name. |
| `PRINCIPAL` |  |  | The principal's name. |

## `tessera grant`

```text
tessera grant --control <ADDRESS> <--principal <NAME>|--group <NAME>> <--term <TERM>|--permission <NAME>>
```

Grant a term or a permission to a principal or a group, on the control plane.

A term says what the grantee may see, and a permission what it may do. The sessions the grant affects end, so they pick it up when they authorise again. The credential is read as `tessera principal` reads it.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `--principal` | `NAME` |  | The local principal the grant is made to. |
| `--group` | `NAME` |  | The group the grant is made to. |
| `--term` | `TERM` |  | A term, which says what the grantee may see. Repeatable: the terms are granted or revoked in one change, and one refused term refuses them all. |
| `--permission` | `NAME` |  | A permission, which says what the grantee may do: `read`, `write`, `authorise-as`, `admin`, `read-all` or `write-all`. |

## `tessera revoke-grant`

```text
tessera revoke-grant --control <ADDRESS> <--principal <NAME>|--group <NAME>> <--term <TERM>|--permission <NAME>>
```

Revoke a term or a permission from a principal or a group, on the control plane. The credential is read as `tessera principal` reads it.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `--principal` | `NAME` |  | The local principal the grant is made to. |
| `--group` | `NAME` |  | The group the grant is made to. |
| `--term` | `TERM` |  | A term, which says what the grantee may see. Repeatable: the terms are granted or revoked in one change, and one refused term refuses them all. |
| `--permission` | `NAME` |  | A permission, which says what the grantee may do: `read`, `write`, `authorise-as`, `admin`, `read-all` or `write-all`. |

## `tessera provider`

```text
tessera provider <COMMAND>
```

Manage OIDC providers on the control plane. The credential is read as `tessera principal` reads it.


## `tessera provider list`

```text
tessera provider list --control <ADDRESS>
```

List every OIDC provider, declared through the API or in `tessera.toml`.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |

## `tessera provider show`

```text
tessera provider show --control <ADDRESS> <NAME>
```

Show one provider.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The provider's name. |

## `tessera provider put`

```text
tessera provider put [OPTIONS] --control <ADDRESS> --issuer <ISSUER> --audience <AUDIENCE> --jwks-url <URL> <NAME>
```

Declare an OIDC provider, or replace one whole. Every session authorised through a provider it replaces ends.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The provider's name. |
| `--issuer` | `ISSUER` |  | The `iss` its tokens carry. |
| `--audience` | `AUDIENCE` |  | The `aud` its tokens must hold. |
| `--jwks-url` | `URL` |  | Where its signing keys are published: `https`, or `http` to a loopback address. |
| `--claim-rule` | `CLAIM TEMPLATE` |  | A claim rule: a claim path and a template, such as `groups[*] {value}`. Repeatable. |
| `--role-mapping` | `CLAIM VALUE GROUP` |  | A role mapping: a claim path, the exact value, and the local group it gives, such as `groups[*] tessera-admins admins`. Repeatable. |

## `tessera provider delete`

```text
tessera provider delete --control <ADDRESS> <NAME>
```

Remove a provider. Every session authorised through it ends.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--control` | `ADDRESS` |  | The control plane's address: `http://<host>:<port>`, or `unix:<path>` for a Unix socket. |
| `NAME` |  |  | The provider's name. |
