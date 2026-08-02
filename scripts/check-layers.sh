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
# lifecycle §7's sync-engine rule, made mechanical (D-D): the engine's intra-request parallelism
# is rayon's plain-thread pool, never tokio — an async runtime inside a supposedly synchronous
# engine would reintroduce exactly the reactor-blocking hazard D-A moved off the server's own
# reactor. rayon itself is fine and expected.
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
# I7. The candidate-list route is DECLINED (decision 0008) and the reasoning lives in
# `select.rs`'s module doc, because it warrants a comment in the source and not just a line in a
# document. A comment CI cannot notice being deleted is a
# comment that will be deleted -- and the deletion this guards against ("simplify: drop the
# direct path") reintroduces tippecanoe's empty-tile cliff silently, for the sparsest principals.
if ! grep -q "NO CANDIDATE-LIST ROUTE" crates/tessera-engine/src/select.rs; then
  echo "FAIL: select.rs has lost the 'NO CANDIDATE-LIST ROUTE' block (I7, plan §6)"
  fail=1
fi

# Three rules, each guarding a property that is structural TODAY and stays that way only while
# nothing new is added beside it. Each is DEMONSTRATED going red -- planted, run, reverted --
# because an unfalsifiable rule reads as evidence while providing none, which is how rule 2 came
# to exempt the only two manifests it existed to police.

# 1. ONE PUBLISHER. A generation swap written as `load_full` + `store` loses a concurrent
#    geometry publication -- leaving the LIVE generation on the pin drain list, where the cache's
#    prune evicts projections still in use. Routing every swap through the single executor thread
#    closes that structurally. Flush would be precisely a second publisher, and lifecycle §1.3
#    already requires it to submit a command rather than store directly ("a swap-only publication
#    step" on the lifecycle thread).
#
#    Matches the CALL FORM, and matches it BROADLY. Two narrower spellings were tried and both
#    were vacuous: `GenerationHandle::store` appears nowhere in the tree (both real sites are
#    `self.generation.store(...)`), and `.store(Arc::new(` misses the equally valid
#    `.store(std::sync::Arc::new(` -- verified by planting one and watching this rule stay green.
#    So the rule flags EVERY publishing CALL FORM in the engine's sources outside `write.rs`,
#    minus the atomic ones, which are told apart by the `Ordering::` argument that `Atomic*::store`
#    requires and `ArcSwap::store` does not take. A rule that cannot go red is worse than no rule,
#    because it is evidence.
#
#    FOUR call forms, not one. `.store(` alone was vacuous against three of the four ways arc-swap
#    publishes -- `.swap(` and `.rcu(` were both planted in `viewport.rs`, both compiled, and both
#    left this rule green. `rcu` matters most: it is the read-modify-write spelling arc-swap's own
#    documentation recommends, so it is exactly what a stage-2.2 flush author reaches for while
#    "properly fixing" the race. `.swap(` on a slice or a `Vec` would false-positive here; there is
#    no such call in this crate today, and a mechanical `.swap(i, j)` is a one-line exclusion to
#    argue at review, which is the right cost for keeping the publishing form covered.
#    THE EXEMPTION IS SPENT, AND THE RULE IS NOW UNCONDITIONAL (#59, 2026-08-02). There was one
#    marker: `Engine::publish_geometry`'s compare-and-swap in `session.rs`, which existed because
#    nothing else moved `segments_version` and without it the drain list was untestable. It named
#    its own retirement condition -- lifecycle §1.3 requires a flush's swap-only publication step to
#    run on the lifecycle thread -- and that is what happened: publication is an `ExecutorWork`
#    variant the executor performs, `Engine::publish_geometry` is a blocking submission, and there
#    is no publisher outside `write.rs` at all. The marker-counting rule went with the marker, per
#    its own instruction. There is no supported way to publish from another thread, so a new marker
#    is not an exemption to argue for -- it is the defect.
if grep -rnE '\.(store|swap|rcu|compare_and_swap)\(' --include=*.rs crates/tessera-engine/src/ \
     | grep -v '^crates/tessera-engine/src/write\.rs:' \
     | grep -v 'Ordering::'; then
  echo "FAIL: a generation is published outside crates/tessera-engine/src/write.rs."
  echo "      The executor thread is the sole publisher (lifecycle §1.3, #59); a second publisher"
  echo "      reintroduces the lost-update race, in which a lost publication strands the LIVE"
  echo "      generation on the pin drain list and a later prune evicts projections still in use."
  echo "      Submit an ExecutorWork::PublishGeometry instead."
  fail=1
fi

# 2. FAULT INJECTION STAYS OUT OF SHIPPED BUILDS. `fault-injection` is enabled only through a self
#    dev-dependency, and `cargo build` does not build dev-dependencies -- so nothing `cargo build`
#    produces can carry it. That property is worth exactly as much as the "only via dev-deps" part,
#    so assert it rather than document it. (Measured caveat, stated honestly in the feature's own
#    comment: `cargo test --workspace` DOES unify the feature across the workspace, exactly as
#    `bench-timing` does. This rule guards the release path, which is the one that matters.)
#
#    ASK CARGO, DO NOT GREP THE MANIFEST. The first version of this rule read manifest text and
#    exempted any manifest containing a `^fault-injection = ` line -- i.e. it exempted, wholesale,
#    `tessera-lifecycle/Cargo.toml` and `tessera-engine/Cargo.toml`, the only two manifests that
#    could ever acquire a normal dependency enabling the feature. Demonstrated, not inferred:
#    setting tessera-engine's `tessera-lifecycle` dependency to `features = ["fault-injection"]`
#    left the rule green while `cargo tree -e normal -p tessera-cli` showed the switchboard reaching
#    the release binary. The resolved feature set is the only thing that can answer this question,
#    and `-e normal` is what excludes the self dev-dependency edge the gate is built on.
if cargo tree -e normal --workspace -f "{p} {f}" | grep -n 'fault-injection'; then
  echo "FAIL: 'fault-injection' is enabled on a NORMAL dependency edge (lines above)."
  echo "      The gate's whole guarantee is that cargo build cannot reach it, and a normal edge"
  echo "      puts the fault switchboard in the shipped binary. It may be enabled ONLY through the"
  echo "      self dev-dependency in tessera-lifecycle/tessera-engine's [dev-dependencies]."
  fail=1
fi

# 3. THE ACK PROOF HAS ONE HOME. `write.rs`'s `Published` token is what `Executor::ack` demands
#    before it will send a *successful* receipt, and the ack-ordering fail-open it guards
#    (lifecycle §4: a client holding 200 for a suppression not yet in force) is reintroduced by any
#    code that can mint one. The token's own module argues the residual hole honestly -- inside
#    `write.rs` a `Published::already_in_force(..)` call is still reachable, which is exactly what
#    the reviewer's mutation used -- so pin construction to that file and keep the count auditable.
#    A new crate or module minting proofs is the change this refuses.
if grep -rn 'Published::' --include=*.rs crates/ | grep -v '^crates/tessera-engine/src/write\.rs:'; then
  echo "FAIL: the ack proof token is constructed outside crates/tessera-engine/src/write.rs."
  echo "      Only the generation swap (and contracts 3.4 replay) may produce one; see the"
  echo "      'mod ack' block in write.rs. A third producer is the guarantee gone."
  fail=1
fi

# 4. THE FRAGMENTATION COUNTERS STAY OFF THE VIEWPORT PATH. They are an operator gauge and nothing
#    else: contracts 3.4 says outright that no request-path behaviour depends on them. Both
#    accessors take an `ExecutorStats` snapshot, which reads a mutex the write executor holds at
#    every window close -- so a viewport that consulted one would put a read request behind the
#    write path for a number it has no business having. The rule polices NAMING, not cost: a
#    request path that wants one of these numbers has to make it reachable under this name, and the
#    grep is what makes that visible in review.
#
#    Demonstrated red by planting `self.write_executor_stats().run_ratio()` in `viewport.rs` --
#    which compiles, because both are public and `viewport.rs` is in the same crate -- running,
#    and reverting.
if grep -n 'run_ratio\|postings_per_container\|fragmentation' \
     crates/tessera-engine/src/viewport.rs \
     crates/tessera-engine/src/select.rs \
     crates/tessera-engine/src/compose.rs \
     crates/tessera-server/src/viewer.rs \
     crates/tessera-server/src/session.rs; then
  echo "FAIL: a fragmentation counter is named on the viewport/session path (lines above)."
  echo "      Contracts 3.4: no request-path behaviour depends on this figure. It is emitted on"
  echo "      the bearer-gated /control/status and read there only."
  fail=1
fi

exit $fail
