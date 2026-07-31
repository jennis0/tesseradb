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
# Track B, Task 3a. Two rules, both guarding a property that is structural TODAY and stays that
# way only while nothing new is added beside it.

# 1. ONE PUBLISHER. Track C's finding S2: `write.rs`'s generation swaps use `load_full` + `store`,
#    which loses a concurrent geometry publication -- leaving the LIVE generation on the pin drain
#    list, where Task 5's prune evicts projections still in use. Task 3a closes it structurally by
#    moving every swap onto the single executor thread. Stage 2.2's flush is precisely a second
#    publisher, and lifecycle §1.3 already requires it to submit a command rather than store
#    directly ("a swap-only publication step" on the lifecycle thread).
#
#    Matches the CALL FORM, and matches it BROADLY. Two narrower spellings were tried and both
#    were vacuous: `GenerationHandle::store` appears nowhere in the tree (both real sites are
#    `self.generation.store(...)`), and `.store(Arc::new(` misses the equally valid
#    `.store(std::sync::Arc::new(` -- verified by planting one and watching this rule stay green.
#    So the rule flags EVERY `.store(` in the engine's sources outside `write.rs`, minus the
#    atomic ones, which are told apart by the `Ordering::` argument that `Atomic*::store` requires
#    and `ArcSwap::store` does not take. A rule that cannot go red is worse than no rule, because
#    it is evidence.
if grep -rn '\.store(' --include=*.rs crates/tessera-engine/src/ \
     | grep -v '^crates/tessera-engine/src/write\.rs:' \
     | grep -v 'Ordering::'; then
  echo "FAIL: a generation is published outside crates/tessera-engine/src/write.rs."
  echo "      Stage 2.1 made the executor thread the sole publisher (Track C finding S2); a second"
  echo "      publisher reintroduces the lost-update race. Submit a Command instead (lifecycle §1.3)."
  fail=1
fi

# 2. FAULT INJECTION STAYS OUT OF SHIPPED BUILDS. `fault-injection` is enabled only through a self
#    dev-dependency, and `cargo build` does not build dev-dependencies -- so nothing `cargo build`
#    produces can carry it. That property is worth exactly as much as the "only via dev-deps" part,
#    so assert it rather than document it. (Measured caveat, stated honestly in the feature's own
#    comment: `cargo test --workspace` DOES unify the feature across the workspace, exactly as
#    `bench-timing` does. This rule guards the release path, which is the one that matters.)
for m in crates/*/Cargo.toml; do
  if awk '/^\[dev-dependencies\]/{d=1} /^\[/{if ($0 !~ /dev-dependencies/) d=0} !d' "$m" \
       | grep -q 'fault-injection'; then
    if ! grep -q '^fault-injection = ' "$m"; then
      echo "FAIL: $m enables 'fault-injection' outside [dev-dependencies]"
      echo "      The gate's whole guarantee is that cargo build cannot reach it."
      fail=1
    fi
  fi
done

exit $fail
