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

exit $fail
