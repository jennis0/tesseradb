<!-- Generated from crates/tessera-cli/src/main.rs. Edit the help text there, then run: TESSERA_WRITE_CLI_REFERENCE=1 cargo test -p tessera-cli cli_reference -->

# CLI

`tessera` is one binary with a subcommand for each job: `build` makes a bundle from a corpus declaration, `check` tests the declaration against its source files, `verify` checks a built bundle, `tokenise` shows how an analyser splits text, and `serve` serves the bundle. `tessera <subcommand> --help` prints the text on this page, and `tessera --version` prints the commit the binary was built from.

`build`, `check` and `serve` read the deployment file `tessera.toml` from the working directory, or from the nearest directory above it that has one. `--deployment` names a different file.

## `tessera build`

```text
tessera build [OPTIONS]
```

Build a bundle from the corpus declaration and the source files it names.

`tessera build` needs no flags. It reads `tessera.toml` from the working directory, or from the nearest directory above it that has one. That file names the corpus declaration in `[build] schema` (default `schema.toml`) and the bundle directory in `[bundle] path`, each relative to the file's own directory. The declaration names the source files.

The identity key comes from the environment variable that `[identity] env` in `tessera.toml` names, `TESSERA_IDENTITY_KEY` by default. A `.env` file beside `tessera.toml` may set it, and a value in the process environment takes precedence over the file. With no key, the build is refused before it reads any data; pass `--carry-id-key-from`, `--identity-file` or `--mint-id-key` to supply or create one.

The other flags override `tessera.toml` or tune the build.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
| `--out` | `PATH` |  | Write the bundle to this directory instead of `[bundle] path` in `tessera.toml`. `tessera serve` opens `[bundle] path`, so it does not serve a bundle written elsewhere. |
| `--limit` | `ID` |  | Build only the rows whose identity column's value is an integer below this value. A negative value counts as its unsigned 64-bit value, at least 2^63, so the limit drops it. Refused when the identity column holds strings or bytes, and when rows have no identity column. Layer member files are read whole: a member row naming a row the limit dropped is refused, so limit the member file to the same values. |
| `--config` | `PATH` |  | Read this corpus declaration instead of `[build] schema` in `tessera.toml`. The declaration is compiled into the bundle's `MANIFEST.json`, and the server reads it from there. |
| `--file` | `NAME=PATH` |  | Read the source NAME in the declaration's `[sources]` from PATH instead. Repeatable. A relative PATH is read from the working directory, not from the declaration's directory. Every block that reads the source reads PATH. Refused when `[sources]` has no source called NAME (the message lists the names it has), and when NAME is given twice. The flag replaces a source's path and cannot add a source. |
| `--mint-external-ids` |  |  | Write an external id for every item, made from its identity column's integer value. An identity column of strings or bytes writes external ids without this flag. Rows with no identity column get no external ids, with or without it. |
| `--no-oracle-pairs` |  |  | Do not write `pairs.parquet`. The server does not read the file. The conformance suite and `tessera verify --deep` do, and `verify --deep` passes a bundle without one. |
| `--batch-items` | `ITEMS` | derived | Assign internal ids in batches of this many items. Default: the largest batch the memory budget allows, which is the whole corpus when it fits. Refused when the batch does not fit the budget, or when it is less than half the size the budget allows. A build of more than one batch records the size in the bundle, and a different size assigns different internal ids. `--carry-id-key-from` reuses a recorded size and refuses a different one. When the carried bundle was built as one batch, a size given here is used, and the build prints a note saying the ids will differ. |
| `--memory-budget` | `SIZE` | derived | Peak memory for the build's own structures, in bytes or with a `k`, `m` or `g` suffix, such as `24g`. Default: 80% of the available memory or of the process's cgroup limit, whichever is lower, kept between 2 GiB and 1 TiB, and 24 GiB where available memory cannot be read. Batch and band sizes follow from it. A build that would not fit is refused before it assigns internal ids, with the arithmetic in the message. |
| `--stage-timings` |  |  | Print each build stage's wall time, row count and peak resident memory to stderr as the stage ends. Peak resident memory is the process's high-water mark when the stage ended, so it shows the stage in which the peak was reached. |
| `--stage-timings-json` | `PATH` |  | Write the per-stage records to this file as JSON when the build ends, including a build that fails. One object per stage, in order, with `stage`, `wall_s`, `rows`, `peak_rss_kib`, `started_at` and `ended_at`. It does not need `--stage-timings`. |
| `--carry-id-key-from` | `BUNDLE` |  | Reuse the identity key, idset and batch size recorded in an existing bundle. This rebuilds a bundle while every `tessera_id` a client holds stays valid. Refused when the bundle's identity construction differs from this binary's, and when its key disagrees with another key source unless `--rotate-id-key` is given. |
| `--identity-file` | `PATH` |  | Read the identity key from a TOML file with an `[identity]` table holding `key`, 32 lowercase hex digits, and an optional `idset`. A file can be made readable by its owner only, which an environment variable is not: another process of the same user can read it from `/proc`. Refused when `[identity]` has any other key or an idset of 0. |
| `--mint-id-key` |  |  | Generate a new random identity key at idset 1 and print it. This starts a new identity: every `tessera_id` a client holds becomes invalid. Record the printed key, in the environment variable or a `.env` file beside `tessera.toml`, so later builds can reuse it. `--idset` and `--bump-idset` are ignored: the idset is 1. Refused when any other key source is given or set. |
| `--rotate-id-key` |  |  | Accept a change of identity key when the key sources disagree. Without it, disagreeing sources are refused. The new key is the one from `--identity-file` if given, otherwise the one from the environment. Every `tessera_id` a client holds becomes invalid, every row is stored in a new order, and the idset returns to 1 unless `--idset` is given. |
| `--bump-idset` |  |  | Add one to the idset and keep the key. The server publishes the idset at `/v1/meta`, and refuses a request that names any other idset. |
| `--idset` | `N` | derived | Set the idset. Default: the idset in `--identity-file`, then the one recorded by `--carry-id-key-from`, then 1. Required when those two disagree. `--bump-idset` adds one to it. |

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

Verify a bundle's files and its identity column.

It opens the bundle as the server does. That checks every manifest digest, the size and SHA-256 of every file except the external-id files and their locator, and that each segment's permutation maps one-to-one onto its rows. `--deep` hashes the external-id files too. It then confirms that the row space holds exactly the rows the segments claim, and computes each row's `tessera_id` again from the identity key, failing on the first row that differs.

It also prints to stderr each indexed keyword column's count of distinct values against its rows, with a warning for a column whose values are unique per row. A warning does not fail the verify.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `BUNDLE` |  |  | The bundle directory, the one holding `CURRENT`. |
| `--deep` |  |  | Also check the bundle's internal structures. The external-id files and their locator are hashed against the manifest. The term lists must be sorted, free of duplicates and in range; the external-id index and its locator must agree in both directions; dictionary records must not repeat; record blobs and Morton cells must agree with their indexes; each group-scoped render column must be present in every segment; and `pairs.parquet`, when present, must match the term lists it was written with. Run it on a bundle no running server is writing to. |

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

## `tessera serve`

```text
tessera serve [OPTIONS]
```

Serve the bundle that `tessera.toml` names.

Finds `tessera.toml` as `tessera build` does, opens the bundle at `[bundle] path` and the write-ahead log, and listens on the viewer, session and control addresses in `[serve]`. When all three are bound it prints one line of JSON to stdout naming them. Diagnostics go to stderr.

It refuses to start when `tessera.toml` is refused, as `tessera build` would refuse it; when the session or operator credential is not set, or its file cannot be read (set `session_credential_file` or `session_credential_env`, and the same for `operator`, under `[serve]`); when a `[serve]` address is missing; when the bundle cannot be read; and when the write-ahead log fails its checksum.

| Argument | Value | Default | Description |
| --- | --- | --- | --- |
| `--deployment` | `PATH` |  | Read this `tessera.toml` instead of searching for one upward from the working directory. |
