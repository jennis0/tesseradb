# CLAUDE.md

## What this is

Tessera serves an interactive map over billions of documents or records from one machine, to many viewers, while the corpus changes underneath it. Each viewer sees the map computed over exactly the items they may see: every count, density, cluster, label and sample, not only which items they can retrieve.

A viewer's visible set is computed once per session as a Roaring bitmap. Geometry is stored in Morton order, so a tile is a contiguous range of row ids and a masked count is bitmap arithmetic.

[docs/system/](docs/system/) describes the system; start at [overview.md](docs/system/overview.md). [docs/openapi/](docs/openapi/) is the HTTP contract. The code is the authority on everything else. If a comment cites a document or a decision number that is not in the tree, ignore the citation and read the code.

## Layout

One Rust workspace under [crates/](crates/), one binary (`tessera`).

- `tessera-build` turns source files into a bundle. `tessera-engine` opens a bundle, answers requests and takes writes. `tessera-server` is the HTTP layer over the engine. `tessera-store` is the on-disk formats. `tessera-lifecycle` is the write-ahead log and the write commands.
- A build is an ingest into an empty database. A rule about what may be declared or stored is written once, below both, and called by both.
- [clients/py/](clients/py/) is the Python SDK and [clients/ts/](clients/ts/) the browser client. Python is a consumer and is never in a request path. [conformance/](conformance/) is a Python suite that checks a running server against an independent oracle.
- [probes/](probes/) and `tessera-bench` are measurements. Re-run a figure before relying on it.

Nothing is deployed, so there is no backwards compatibility: change a format and rebuild the bundles. Bump the format version when you do, so a stale bundle is refused.

## The security boundary

[docs/system/security.md](docs/system/security.md) is the threat model. The mistakes a reasonable-looking change most often makes:

- Computing an aggregate over the whole dataset and then filtering it. Every count, histogram and cluster is computed from inside the viewer's visible set.
- Sampling before masking. A viewer with few visible items then gets a blank map and no error.
- Gating a label on the filtered set. Labels gate on the authorised set; a filter can hide things and can never reveal them.
- Sending an entity id to a client. Clients see `tessera_id`, a blinding permutation. It is not encryption.
- Removing a deny early. A suppression is removed only when it is lifted; a deletion only by the compaction that removes its rows. A suppression applies to every request from the moment it is accepted.

Refuse or withhold only where something would leak to a viewer or cannot be undone (entity ids, term ids, a published identity). Everywhere else the user decides: do what they asked, report what happened with numbers, and do not add a refusal, a warning or a default on their behalf.

## Working here

- Fix the cause where it is. If the server lacks something a client needs, change the server.
- Prefer deleting to adding. Before writing a helper, a check or a type, look for the one that exists.
- Comments say what the code does not. No history, no citations, no argument for the design. An error message says what is wrong and what to write instead, in a sentence.
- Test behaviour through the public surface: what is stored, what is served, what survives a restart. Do not assert message text or the shape of internals. A rule's tests live beside the rule.
- [docs/writing.md](docs/writing.md) is the style for prose. British spelling.

## Talking to Joe

He knows the system better than you do and is usually thinking out loud. Answer what he asked, in ordinary English, and leave the decision with him. When a change alters what a user can do, ask him with short lettered options. Agreement is a complete reply.

## Checks before finishing

```bash
cargo test --workspace --no-fail-fast
cargo clippy --workspace --all-targets -- -D warnings
bash scripts/check-layers.sh
bash scripts/check-test-reachability.sh --quick
bash scripts/check-clients.sh
bash clients/py/check.sh
```

Keep `--no-fail-fast` and read the totals: without it cargo stops at the first failing binary and a smaller passing count looks green. Build with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` in a worktree; debug targets are tens of gigabytes each.
