#!/usr/bin/env bash
# scripts/check-layers.sh — dependency edges the design forbids (SA §3).
# DIRECT dependencies only: server -> engine -> store is legitimate transitively,
# so a transitive check would be permanently red. --depth 1 is load-bearing.
set -euo pipefail
fail=0
deny() { # deny <crate> <forbidden-DIRECT-dep>
  if cargo tree -p "$1" --prefix none -e normal --depth 1 | tail -n +2 | grep -q "^$2 "; then
    echo "FORBIDDEN: $1 directly depends on $2"; fail=1
  fi
}
deny tessera-authz tessera-store
deny tessera-authz tessera-spatial
deny tessera-server tessera-store     # server sees engine API types only
deny tessera-server tessera-authz
deny tessera-wire tessera-store
deny tessera-wire tessera-authz
# lifecycle §7's sync-engine rule, made mechanical (D-D/Task 6): the engine's intra-request
# parallelism is rayon's plain-thread pool, never tokio — an async runtime inside a supposedly
# synchronous engine would reintroduce exactly the reactor-blocking hazard Task 3 (D-A) moved off
# the server's own reactor. rayon itself is fine and expected (the whole point of this task).
deny tessera-engine tokio
deny tessera-store tokio
# I4: no ID conversions in types
if grep -rn "impl From" crates/tessera-types/src/ | grep -E "EntityId|RowId|TermId|TesseraId|Handle"; then
  echo "FORBIDDEN: ID conversion in tessera-types"; fail=1
fi
# I10: the payload module never sees EntityId (handles + plain columns only)
if grep -n "EntityId" crates/tessera-wire/src/payload.rs 2>/dev/null; then
  echo "FORBIDDEN: EntityId in tessera-wire payload module"; fail=1
fi

# I10 (contracts r6): no request-path artifact stores an entity ID. `columns.arrow`
# carries `tessera_id`, so the store's column reader must expose no entity_id accessor.
if grep -n "fn entity_id" crates/tessera-store/src/read.rs; then
  echo "FAIL: ColumnsRef exposes an entity_id accessor; contracts r6 removed the column"
  fail=1
fi

# The identity key inverts every tessera_id and must never reach the wire.
if grep -rn "IdentityKey" crates/tessera-wire/src/ crates/tessera-server/src/viewer.rs; then
  echo "FAIL: the identity key must not appear in the wire or viewer layers"
  fail=1
fi

# The grep above cannot see the key's *plaintext hex*, which is a plain `String` carried beside
# the redacted `IdentityKey` (seam finding S3). Name its carrier explicitly.
if grep -rn "identity_key_hex" crates/tessera-wire/src/ crates/tessera-server/src/; then
  echo "FAIL: the identity key's plaintext hex must not appear in the wire or server layers"
  fail=1
fi

# Do NOT add a `priority` grep here (2026-07-30 fold). `priority` is `high16(tessera_id)` -- a
# keyed prefix of a value the payload already carries in full -- so publishing it discloses
# nothing beyond the id itself, and Important I-4 (which this would-be grep enforced) is
# retired. See task-10-brief.md's "The routing principle" note before reinstating anything here.

# `tessera-bench` sits ABOVE every other crate: it reaches across authz + store + spatial +
# engine + build + server together, which no shipped crate may do. The edge must stay one-way, so
# nothing may depend on it.
for c in types plugin authz store spatial lifecycle engine wire server build cli; do
  # Match a dependency declaration (`tessera-bench = ...` or a path to it), not prose -- these
  # manifests discuss the harness in comments, and a substring grep flags its own documentation.
  if grep -nE '^[[:space:]]*tessera-bench[[:space:]]*=|\.\./tessera-bench' "crates/tessera-$c/Cargo.toml" >/dev/null 2>&1; then
    echo "FAIL: tessera-$c depends on tessera-bench; the measurement harness must stay a leaf"
    fail=1
  fi
done

# The stage-timing header must not exist in a shipped binary. `tessera-bench` enables
# `bench-timing` by default, and cargo unifies features across a `--workspace` build, so the
# compile gate alone is not enough -- `tessera-server` also gates emission on `[serve]
# stage_timing`, default false. Assert the runtime gate is still there and still defaults closed.
if ! grep -q "stage_timing" crates/tessera-server/src/config.rs; then
  echo "FAIL: the [serve] stage_timing runtime gate is missing from the server config"
  fail=1
fi
if ! grep -q "stage_timing.unwrap_or(false)" crates/tessera-server/src/config.rs; then
  echo "FAIL: stage_timing must default to false (fail closed)"
  fail=1
fi

# ---------------------------------------------------------------------------------------------
# Track A, Task 2. I7 / plan §6: "This warrants a comment in the source, not just a line in a
# document." The candidate-list route is DECLINED (Phase 2 roadmap, owner ruling 1) and the
# reasoning lives in `select.rs`'s module doc. A comment CI cannot notice being deleted is a
# comment that will be deleted -- and the deletion this guards against ("simplify: drop the
# direct path") reintroduces tippecanoe's empty-tile cliff silently, for the sparsest principals.
if ! grep -q "NO CANDIDATE-LIST ROUTE" crates/tessera-engine/src/select.rs; then
  echo "FAIL: select.rs has lost the 'NO CANDIDATE-LIST ROUTE' block (I7, plan §6)"
  fail=1
fi

exit $fail
