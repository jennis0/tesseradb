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
| `--limit` | `VALUE` |  | Build only the rows whose value of the declaration's one unique integer attribute is below this value, in every file that carries its column. A negative value counts as its unsigned 64-bit value, at least 2^63, so the limit drops it, and so does a null. Refused when the declaration has no unique integer attribute, or more than one. A row of a file without the column is read, and one that names an item the limit left out names no item and is refused. |
| `--strict` |  |  | Refuse the build at the first file with a row the identity rule refuses. Without it, a row naming two items, naming an item or a unique value an earlier row of its file names, or, outside a view's points, naming no item, is left out and the build goes on. The refused rows are printed, and written to `reports/refused.json` in the bundle. |
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

The report goes to stderr: the files read, the findings, warnings, the frames the views will have, the view groups, the shape layers' sizes, and on a clean check the disclosure table. The exit status is non-zero when there is a finding; a warning does not change it.

It cannot check anything that needs a row: whether a closed vocabulary covers the values in the data, whether a member id resolves, or where the data lies in its view's extent. `tessera build` reports those.

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
| `--deep` |  |  | Also check the bundle's internal structures. The term lists must be sorted, free of duplicates and in range; dictionary records must not repeat; record blobs and Morton cells must agree with their indexes; each group-scoped render column must be present in every segment; each unique column's index must be hashed against the manifest, name at most one live item for a value and agree with the column's values in both directions; and `pairs.parquet`, when present, must match the term lists it was written with. Run it on a bundle no running server is writing to. |

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
| `--token` | `TOKEN` |  | A session token, as the session plane's `/session/authorise` issues one. Without it the token is read from `TESSERA_TOKEN`, and with neither the read is refused. |
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
| `--token` | `TOKEN` |  | A session token, as the session plane's `/session/authorise` issues one. Without it the token is read from `TESSERA_TOKEN`, and with neither the read is refused. |
| `--out` | `PATH` |  | The file to write. Without it, or with `-`, the output goes to stdout. |
| `--format` | `FORMAT` |  | `ipc` writes an Arrow IPC stream and `parquet` a Parquet file. Without it the format is the extension of `--out`, `.arrows` or `.parquet`; stdout and any other extension need it. A format that disagrees with the extension of `--out` is refused. |

## `tessera serve`

```text
tessera serve [OPTIONS]
```

Serve the bundle that `tessera.toml` names.

Finds `tessera.toml` as `tessera build` does, opens the bundle at `[bundle] path` and replays the write-ahead log at `[bundle] wal`, all before it binds any address. It then listens on the viewer, session and control addresses in `[serve]`. When all three are bound it prints one line of JSON to stdout naming them. Diagnostics go to stderr, in colour only when stderr is a terminal.

SIGTERM or SIGINT stops the server at once, even while it opens the bundle, and it exits 0. Stopping ends every viewer session, because sessions are held in memory. A write is acknowledged only once the write-ahead log holds it on disc, so stopping loses no acknowledged write. A deletion or suppression answered with 500 because the log could not be written is in force but not on disc, and a restart undoes it until the change is sent again.

It refuses to start, and exits 1, when `tessera.toml` is refused as `tessera build` would refuse it or lacks one of the three `[serve]` addresses. It refuses when the session or operator credential is not set or its file cannot be read. Under `[serve]`, `session_credential_file` or `session_credential_env` names the file or environment variable holding the session credential, and `operator_credential_file` or `operator_credential_env` the operator's. It also refuses when the bundle cannot be read and when the write-ahead log fails its checksum. An address that cannot be bound, such as one already in use, stops it with exit 1 after the bundle has opened.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
