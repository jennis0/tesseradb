# The `tessera_id` construction (2026-07-30)

**Status:** specification, for review; **amended 2026-07-30** (see *Amendments*, below).
**Touches:** contracts §1, §2.2, §2.4, §2.6; design
§10.6, §12.5, §13.3, §16, Appendix C (C6, C17); `tessera-types`, `tessera-build`,
`tessera-store`, `tessera-engine`, `tessera-wire`, the reference oracle, the byte-scanner.

**Provenance:** the external-ID identity plan
(`docs/superpowers/plans/2026-07-30-external-id-identity.md`, section *"The identity
construction — specified, not assumed"*), Task 3. This memo is the normative statement;
Task 4 lands it in the contracts spec, Task 5 implements it in Rust and Task 12
reimplements it in Python.

**Why this document exists at all.** The construction has two independent
implementations — Rust (`tessera-types`) and the Python reference oracle — and the
oracle's independence is the only thing that makes their agreement evidence of anything.
So the specification and its known-answer vectors are written **before either
implementation**, and both are then held to the vectors. This memo is therefore written
for one particular reader: **a competent Python programmer with no access to the Rust,
who must reproduce the construction from this text alone and get it right on the first
try.** Anything that reader would have to guess at is a defect in this memo.

The shared gate is `reference/vectors/tessera_id.json`. **Disambiguation rule, agreed in
advance:** if the two implementations disagree with each other but one of them agrees
with the vectors file, the file is authoritative and the disagreeing implementation is
wrong; if both implementations agree with each other and disagree with the file, the file
is wrong and is corrected by a commit that says so.

**Vocabulary, deliberately.** This is a **blinding permutation**, not encryption. It is
called that throughout, and in any spec text derived from this memo, because calling it
encryption would claim a security property §8 explicitly does not claim.

## Amendments since the reviewed version (`acfe2ce`)

Recorded here rather than applied silently, because this memo is the record of a review and
a memo that quietly drops or rewrites a condition misrepresents it. Nothing in §1.1–§1.9
changes, so `reference/vectors/tessera_id.json` is **unamended** and the independent gate on
it (114/114 reproduced from this text alone) still stands.

**From the owner's `priority` decision (2026-07-30), landed in design r21 / contracts r6:**

- **§3.2** — the `priority` viewer-plane prohibition is **retired**, superseded by keying.
  The general rule it was an instance of is kept verbatim: it is what licenses the change.
  The "Open interaction, flagged not resolved" block is deleted, its condition having been met.
- **§5a, precondition 4** — restated as **satisfied differently, not dropped**: the channel is
  closed by keying `priority` rather than by forbidding it, so the 8-round ruling still rests
  on something real.
- **§6** — the sort order becomes `(morton, tessera_id)` with no further tiebreak; the build
  sequence **inverts** (`tessera_id` is computed before the tiler, not written at the row
  after it); row order is now **key-dependent**, so rotation reorders tied rows. §2.1's
  `--rotate-id-key` row and §3.2's rotation bullet say so too. The storage argument that
  follows is unchanged in substance and gains one sentence at the join.

**From the independent gate on this memo (2026-07-30):**

- **§3** — the three unqualified "cannot" claims are restated as the property the construction
  is **intended** to provide, with a forward reference to §8, which admits that eight rounds of
  a non-cryptographic mixer should not be assumed to resist an adversary holding pairs.
- **§3** — the viewer-plane setting is recorded: **ciphertext-only with a near-degenerate
  plaintext prior** (`shard_id` always 0, `entity_id` dense from zero under I9), which is
  stronger than "no known pairs" implies and which §3.1's own standard says must be written
  down rather than reconstructed.
- **§9** — the `key_schedule` row no longer claims to catch a non-wrapping `k1 × (i + 1)`
  multiply. It cannot: under the canonical key `k1 × 8` never overflows. The credit moves to
  `secondary_key`, whose `k1` does.
- **Change list** — the contracts §2.6 row updated, having recorded the retired prohibition.

---

## 1. The construction

`tessera_id = FPE_k(shard_id: u32 ‖ entity_id: u32) → u64`: a **balanced Feistel
network**, 8 rounds, 32-bit halves, keyed by a 128-bit per-deployment key. It replaces the
`entity_id` column in `columns.arrow` as the wire identity of a point.

**Independently verified invertible, review round 2 — and not to be changed.** The
verification: the forward round is `(L,R) ← (R, L ^ F(i,R))` and the inverse round is
`(L,R) ← (R ^ F(i,L), L)` applied over the rounds in reverse order; the **balanced** 32/32
split makes the composition a permutation of 2⁶⁴ for *any* round function `F`, so
invertibility does not depend on `F`'s quality; **round-count parity is irrelevant** to
invertibility (there is no unbalanced-split or odd-round hazard here); and the output
packing `(L << 32) | R` is itself a bijection. Section 8 records the ruling that keeps
`splitmix64` at 8 rounds. **Do not change the construction** — not the round count, not
the round function, not the packing, not the key schedule.

### 1.1 Input encoding

```
L₀ = shard_id       (u32)
R₀ = entity_id      (u32)
```

Equivalently: the 64-bit input is `(shard_id as u64) << 32 | entity_id as u64`, split at
bit 32, the high half becoming `L₀`. Both halves are unsigned 32-bit; there is no sign
extension anywhere in the construction. In Python, mask every intermediate: all
`splitmix64` arithmetic is modulo 2⁶⁴ and both halves are modulo 2³².

### 1.2 Key encoding, and the hex-case rule

The key is **16 bytes**. It is written, in MANIFEST and in the deployment config file, as
**exactly 32 lowercase hexadecimal characters** (`0-9a-f`), byte 0 first — that is,
`key_bytes[0]` is the first two characters of the string.

**The hex-case rule is: reject, do not normalise.** Readers accept lowercase only and
reject any other spelling with a typed error naming the reason. They do **not** case-fold.
The reason is that MANIFEST has exactly one canonical form for the key, so a digest over
MANIFEST is stable and two MANIFESTs that differ only in the spelling of the key cannot
exist. `IdentityKey::from_hex` rejects: any string that is not exactly 32 characters, any
character outside `0-9a-f` (including `A-F`), and any leading `0x`, whitespace or
separator.

The two `u64` halves are read **little-endian over the decoded bytes**, not over the text:

```
key_bytes = hex_decode(key)                 # 16 bytes, key_bytes[0] = first two chars
k0 = u64_from_le_bytes(key_bytes[0..8])     # bytes 0,1,…,7
k1 = u64_from_le_bytes(key_bytes[8..16])    # bytes 8,9,…,15
```

Worked example, which is also in the vectors file so an implementation can check its
parser before it checks its rounds — for the canonical test key
`000102030405060708090a0b0c0d0e0f`:

```
k0 = 0x0706050403020100
k1 = 0x0f0e0d0c0b0a0908
```

An implementation that gets `k0 = 0x0001020304050607` has read the halves big-endian; one
that gets `k0 = 0x0f0e0d0c0b0a0908` has swapped the halves. Either error reproduces
nothing in the vectors file.

### 1.3 Degenerate keys are rejected

A key with `k1 == 0` collapses the schedule of §1.4 to a single constant round key for all
eight rounds: `round_key(i) = splitmix64(k0)` for every `i`. The all-zero key does the same
and is additionally the value an uninitialised buffer supplies. Both are still
permutations, so **nothing fails loudly** — the bundle builds, the round-trip tests pass,
and the deployment is running an eight-fold repetition of one round.

**`IdentityKey::from_hex`, `--id-key` and `--id-key-file` therefore refuse `k1 == 0`, and
refuse the all-zero key**, with a typed error naming the reason. (The all-zero key is a
special case of `k1 == 0`; both are named because the error message should say which one
the operator hit.) The CSPRNG mint **retries** rather than emitting a degenerate key —
probability ~2⁻⁶⁴, so the retry loop exists so that the property is *enforced* rather than
*assumed*. The vectors file carries these rejections as cases (§9).

### 1.4 Key schedule

```
round_key(i) = splitmix64( k0 ^ (k1 *64 (i + 1)) )     for i = 0, 1, 2, 3, 4, 5, 6, 7
```

where `*64` is wrapping (modulo 2⁶⁴) multiplication and `i + 1` is computed as a `u64`
(so the multiplier takes the values `1, 2, 3, 4, 5, 6, 7, 8`). In Python:
`splitmix64(k0 ^ ((k1 * (i + 1)) & 0xFFFFFFFFFFFFFFFF))`.

The eight round keys for the canonical test key are listed in
`reference/vectors/tessera_id.json` under `key_schedule.round_keys`, so an implementation
can localise a key-schedule error without having to bisect the round loop.

### 1.5 Round function

`splitmix64` is exactly the function contracts §2.6 already fixes (all arithmetic wrapping
`u64`, all shifts logical right shifts on unsigned values):

```
splitmix64(x):
    z = x + 0x9E3779B97F4A7C15          # wrapping
    z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9    # wrapping
    z = (z ^ (z >> 27)) * 0x94D049BB133111EB    # wrapping
    return z ^ (z >> 31)
```

The shift amounts are **30, 27, 31**, in that order, and the round function takes the
**high** 32 bits of the result:

```
F(i, r: u32) -> u32 = ( splitmix64( (r as u64) ^ round_key(i) ) >> 32 ) as u32
```

Note that `r` is widened to `u64` and XORed against the *whole* 64-bit round key before
mixing, and that the `>> 32` selects the high half. Taking the low half instead
(`& 0xFFFFFFFF`) is still a valid Feistel and still a bijection — and disagrees with every
vector in the file. Six `splitmix64` known answers are in the vectors file under
`splitmix64` so that a wrong shift constant is caught before the rounds are.

### 1.6 The rounds — forward

The round index takes the eight values `0, 1, 2, 3, 4, 5, 6, 7`, **in that order**:

```
L = shard_id
R = entity_id
for i in 0, 1, 2, 3, 4, 5, 6, 7:          # eight rounds, ascending
    (L, R) = (R, L ^ F(i, R))
tessera_id = (L as u64) << 32 | R as u64
```

**The round enumeration is written out longhand deliberately** *(review round 2
reproducibility edit)*. Rust's `0..8` and Python's `range(8)` both denote exactly these
eight values; a range written `0..=8` gives **nine** rounds and a silently different,
still-invertible, still-collision-free permutation that disagrees with every stored
column. The assignment inside the loop is **simultaneous**: the new `L` is the old `R`,
and the new `R` uses the *old* `L` and the *old* `R`. An implementation that overwrites
`L` first computes `R ^ F(i, R)` and is wrong.

### 1.7 The rounds — inverse

The engine's only use of the inverse is `/v1/items` drill-down: it turns a client's
`tessera_id` back into `(shard_id, entity_id)`.

```
L = (tessera_id >> 32) as u32
R = tessera_id as u32                     # low 32 bits
for i in 7, 6, 5, 4, 3, 2, 1, 0:          # the same eight rounds, descending
    (L, R) = (R ^ F(i, L), L)
shard_id = L
entity_id = R
```

Again the assignment is simultaneous, and again the enumeration is longhand: the indices
are the same eight values in the reverse order. `for i in (0..8).rev()` in Rust and
`for i in reversed(range(8))` in Python both denote this. An off-by-one here —
`7, 6, …, 1` or `8, 7, …, 1` — produces a function that is still a permutation but is not
the inverse of §1.6, so `invert(forward(x)) ≠ x`; the round-trip test in every
implementation must therefore be over *both* functions and not merely a self-consistency
check of one.

**Every `tessera_id` inverts.** The inverse is total over 2⁶⁴: an arbitrary `u64` inverts
to *some* `(shard_id, entity_id)`, and the result is only meaningful if that shard is this
deployment's and that entity exists. A caller-supplied `tessera_id` must therefore be
validated *after* inversion — shard equal to `identity.shard_id`, entity at or below the
allocator high-water and present — and the failure is a `404`, fail-closed, never a
guess. The vectors file's `inverse_only` cases exercise exactly this: arbitrary `u64`s
whose inversions land on arbitrary shards.

### 1.8 `forward`'s input is a checked conversion, not a cast

*(plan Important I-1.)* `entity_id` is `u32` by D8, but `EntityId` is a `u64` newtype in
the code today. `forward` therefore takes an `EntityId` and **returns an error if
`entity.raw() > u32::MAX`** rather than truncating. A truncating `as u32` is what would
make "collision-free by construction" *false*: two entities differing only above bit 32
would share a `tessera_id`, and `invert` would return the **wrong** entity — a
`/control/changes` suppression applied against the wrong item. See §5a: the checked
conversion and the allocator cap must land together, because the cap is what makes the
error unreachable in practice and the checked conversion is what makes the cap's absence
loud rather than silent.

### 1.9 Verified empirically, not only structurally

*(Task 3 Step 3; the script was throwaway and is not committed.)* For the canonical test
key, `forward` was evaluated over **every** `entity_id` in `0 .. 2²⁴` at `shard_id = 0`:
**16,777,216 distinct outputs, no collisions**, and `invert ∘ forward` was confirmed to be
the identity over the dense range `0 .. 2²⁰` and over 200,000 random `(shard_id,
entity_id)` pairs drawn from the full 2⁶⁴ input space. This proves nothing a Feistel does
not prove structurally; it exists to catch a **transcription error in the round loop**,
which is the realistic failure mode, and it caught none.

---

## 2. Key lifetime and location

**The key MUST be stable across rebuilds.** If it changes, every `tessera_id` changes, and
every client-held identifier — a bookmark, a shared link, a row in a consumer's
database — **silently** breaks: the identifier still parses, still has the right width,
and still inverts to a valid entity. A per-*bundle* key is therefore wrong. The key is
**per-deployment (per-lineage)**.

**State the consequence plainly, because it is what all of this section's machinery is
for:** the failure mode of getting the key decision wrong is not an error, it is a
deployment in which every outstanding identifier now names a different item, discovered by
users rather than by the build. **A stdout warning on a ninety-minute build is not a
gate.** Nobody is watching minute 4 of minute 90.

The key lives in MANIFEST, digest-covered like everything else, as a top-level object:

```json
"identity": {
  "construction": "feistel-splitmix64-v1",
  "rounds": 8,
  "key": "<32 lowercase hex characters>",
  "shard_id": 0,
  "epoch": 1
}
```

**An absent `identity` object is a typed reader error, not a default.** A pre-r6 bundle
fails closed at open; it does not acquire a minted key, a zero key or a legacy path.

### 2.1 The seven `tessera build` flags

**CRITICAL N-1 — a build that has made no explicit key decision REFUSES.** Minting must
be a thing an operator *types*. The earlier draft made minting the default and put the
safe path behind a flag, which is backwards at the one step whose mistake cannot be
undone.

| flag | behaviour |
|---|---|
| *(none of the four sources below)* | **REFUSE.** Exit non-zero **before any work**, with: *"no identity key decision: pass `--carry-id-key-from <bundle>` to keep this deployment's lineage (the normal rebuild), `--id-key-file <path>` to read this deployment's key from its config file, `--id-key <32 hex>` to restore a recorded key, or `--mint-id-key` to start a new lineage — which invalidates every `tessera_id` any client holds."* No bundle is written and no disk is consumed. |
| `--carry-id-key-from <bundle-root>` | Read `identity.key` and `identity.epoch` from that bundle's MANIFEST and carry both forward verbatim. **This is the normal rebuild path.** |
| `--id-key-file <path>` | Read the key from this deployment's config file at the given path (§2.2). **This is where the key lives outside the bundle** — the answer to "the bundle was lost and must be rebuilt from source". |
| `--id-key <32 hex>` | Use the given key (lowercase hex, non-degenerate). For restoring a lineage from a recorded key; `--epoch <n>` may accompany it and defaults to 1. **Discouraged in practice** — a key on a command line reaches shell history, process listings and CI logs; `--id-key-file` exists so this need not be the ordinary route. |
| `--mint-id-key` | **Explicitly** mint a fresh 16-byte key from the OS CSPRNG at `epoch = 1`, record it in MANIFEST, and print it prominently. Help text: *"starts a NEW identity lineage; every `tessera_id` any client holds becomes wrong."* |
| `--rotate-id-key` | Required to proceed when a key was carried or supplied *and* the operator intends a different one. The build refuses without it. |
| `--bump-id-epoch` | Advance `identity.epoch` while keeping the key — the repartitioning/resharding signal (§2a). |

Four of the seven are **sources**; three (`--epoch` aside) are modifiers. `--epoch <n>` is
an accompaniment to `--id-key`, not an eighth source.

**Refusal rules, all fail-closed:**

- `--carry-id-key-from` naming a bundle whose `identity.construction` or `identity.rounds`
  differ from this build's → **refuse**. A silently different construction under the same
  key is the worst outcome available: every identifier changes and the key looks unchanged.
- **Any two key sources given that disagree** — `--id-key-file` against
  `--carry-id-key-from`, `--id-key` against either — → **refuse unless
  `--rotate-id-key`**, whose help text states that all outstanding identifiers are
  invalidated **and that row order changes** *(amended 2026-07-30 — the key is now a sort
  key, §6)*. **Agreement between two sources is not an error** and is in fact the useful
  case: it is how an operator checks that the config file and the previous bundle are the
  same lineage.
- A degenerate key (`k1 == 0`, all-zero) **from any source** → refuse (§1.3).
- A key that is not exactly 32 lowercase hex characters → refuse (§1.2).
- A build whose partitioning or sharding differs from the bundle it carried the key from,
  without `--bump-id-epoch` → refuse (§2a).

### 2.2 The deployment config file — `--id-key-file` *(owner ruling Q6)*

MANIFEST holds the key and `--carry-id-key-from` carries it forward, which covers the
normal rebuild. It does not cover the case that matters most: **the bundle is lost, or the
deployment is rebuilt from source**, and the key has to come from somewhere or every client
identifier silently breaks. The owner has ruled that it comes from a per-deployment
configuration file named on the command line — **not** from an environment variable.

> Owner: *"we'll ultimately need something similar to elastic index configuration."*

So the key is the **first tenant of a deployment config file that will grow to carry more
than the key**. **This memo does not design that file** — one flag, one shape, one sentence
of direction. Phase 1 specifies exactly this and no more:

```toml
# Tessera deployment configuration.
# The identity key is per DEPLOYMENT, not per bundle: every `tessera_id` any client
# holds is derived under it, and changing it invalidates all of them. Back this file
# up wherever the deployment's secrets live and treat losing it as losing the
# deployment's identity lineage.
[identity]
key = "<32 lowercase hex characters>"
```

- **Unknown sections and unknown top-level keys are ignored with a note, not an error** —
  that is what lets a later phase extend the file without breaking a Phase 1 binary. **An
  unknown key *inside* `[identity]` is an error**, because a misspelt `kye =` must not fall
  through to a refusal that reads as "no key given".
- **Every validation `--id-key` gets, `--id-key-file` gets:** exactly 32 lowercase hex
  characters, non-degenerate, typed error naming **the file** and the reason.
- **There is no default search path and no environment variable.** The path is always
  given explicitly. This is what keeps **N-1 intact**: the refusal exists so that a
  *human* decides, and a file the binary finds on its own — in `$CWD`, in `/etc`, in
  `$TESSERA_CONFIG` — is not a human deciding. `--id-key-file` counts as an explicit
  decision **only because the operator typed the path**. An implementation that adds a
  fallback location has defeated N-1 without touching N-1's code. The same reasoning
  applies to a default baked into a `Makefile`, a CI job or `scripts/build_full.sh`: a
  wrapper that supplies a source the operator did not choose has moved the decision out of
  the human's hands, and the refusal cannot see it.
- **`--mint-id-key` prints and does not write.** Writing a config file as a side effect of
  a build would create the very file the previous bullet forbids the binary from finding
  on its own.

**Out of scope, recorded as direction only:** the wider config file's schema, precedence
between file and flags beyond the refusal rule above, per-index or per-partition settings,
and secret management.

---

## 2a. `tessera_id` is a transport identifier, and the identity epoch

`tessera_id` is a **transport** identifier (owner ruling 10). The key makes it stable
across rebuilds. **Nothing makes it stable across a repartitioning or a reshard**, because
the bijection's input encodes placement: §12.5 says a policy change is a reindex, and
§13.3's shards are row ranges. A repartitioning moves *some* points, and every moved
point's `(shard_id, entity_id)` — and hence its `tessera_id` — changes.

**The danger is not a `404`. It is that the churn is *partial*:** an identifier that named
a moved point now inverts, perfectly validly, to whatever entity now occupies that slot. A
client presenting a stale ID gets a `200` describing a **different item**, silently. A
signal is therefore mandatory, not a nicety.

**Where it lives.** §12.5's mitigation is already the right shape: a repartitioning is
built under a new §10.2 immutable versioned prefix and cut over by **flipping a pointer**.
That flip is the natural home for the epoch, because it is the exact moment at which
outstanding identifiers stop meaning what they meant.

**The contract:**

- `identity.epoch: u32` in MANIFEST; carried forward verbatim by `--carry-id-key-from`;
  advanced by `--bump-id-epoch`; **required to be advanced by any build whose partitioning
  or sharding differs from the bundle it carried the key from** — refuse otherwise, on the
  same fail-closed principle as the construction check.
- `GET /meta` reports `identity_epoch`. It is a deployment-level integer, it is not the
  key, it encodes no entity data and it leaks nothing — exactly as sensitive as
  `bundle_format`.
- `/v1/items/{tessera_id}` accepts an **optional** `epoch`. If present and unequal to the
  current epoch, the response is `409`, `detail: "stale identity epoch; re-resolve by
  external_id"`. The branch is entity-independent — the check happens **before** inversion,
  costs a scalar comparison, and returns the same answer for every ID in existence — so it
  is not itself a channel.
- Rotation (`--rotate-id-key`) resets `epoch` to 1: a new lineage, not a continuation.

**Advertised, not required** *(owner ruling Q8)*. The stricter alternative — a mandatory
epoch on every drill-down — was declined, and the reasoning is recorded so a later reader
does not re-derive it badly: (1) **the durable identifier is the `external_id`**, so a
consumer following the contract has no stale `tessera_id` in its database to present; (2)
key rotation and repartitioning are deliberate breaking changes, not scheduled hygiene, so
the epoch fires approximately never, and requiring it would put friction on every
drill-down forever; (3) a required field is also a required *round trip*, making `/meta` a
precondition of the first drill-down for no benefit in the 99.99% case.

**The residual cost of "advertised", recorded not hidden.** A client that ignores the epoch
entirely can, after a repartitioning, present a stale ID and receive a `200` describing a
different item. **That is the caller's choice, made once**, and it is exactly C6's and
C12's shape: the service offers the mechanism and states the consequence. It is cheap to
take — one integer from `/meta`, echoed on drill-down — and any client that holds
identifiers across a deployment change should take it. Task 4 states this in contracts §2.2
*alongside* the mechanism, so that "optional" does not read as an oversight.

**What consumers are told, in one sentence:** *persist `external_id`; treat `tessera_id` as
valid only for the epoch it was issued under.*

---

## 3. Threat model

**The key is not a secret against anyone holding the bundle.** It is in MANIFEST; a
bundle-holder inverts every `tessera_id` to `(shard, entity)` trivially. That is acceptable
and intended: a bundle-holder already has the postings, the masks and the geometry, so
entity IDs tell them nothing new.

**The property being defended is narrower and precise:** the construction is **intended to
provide** that a **viewer-plane client** — holding `tessera_id`s and no bundle — has no
practical route to derive entity IDs, to order them, or to count the gaps between them. That
is what Appendix C's C6 was about, and what D1 relaxes to the caller's own external IDs.

**Stated as an intent rather than as three "cannot"s, deliberately** *(amended 2026-07-30,
raised by the independent gate on this memo)*. §8 says plainly that eight rounds of a
non-cryptographic mixer **should not be assumed to resist** an adversary who obtains known
`(entity_id, tessera_id)` pairs; three unqualified "cannot" claims here would contradict that
admission 260 lines later, and a reader who met only the first would take a stronger guarantee
than this memo makes. What is claimed is the design intent and the absence of a known route
under the setting below — not an unconditional impossibility.

**The viewer-plane setting, written down rather than left to be reconstructed** *(amended
2026-07-30; §3.1's own standard is that a premise a ruling depends on must be recorded)*. The
viewer-plane adversary's position is **ciphertext-only with a near-degenerate plaintext
prior**, and that is *stronger* than "no known pairs" implies in both directions, so both
limbs are recorded:

- **Stronger for the defence than a generic ciphertext-only setting**, because the viewer has
  no pairs at all: the layer check and the byte-scanner keep entity IDs off the plane
  entirely, so the viewer never observes a plaintext.
- **Weaker than a generic ciphertext-only setting**, because the plaintext distribution is
  almost fully known: `shard_id` is **always 0** for as long as §13.4 rules sharding
  premature (§5), and `entity_id` is **dense from zero** under I9's append-only allocation.
  The viewer therefore knows the plaintext set is essentially `{0, 1, …, high_water}` in the
  low half with a constant high half, and — via `/meta`-adjacent corpus scale — approximately
  how large it is. The adversary's task is not "guess the plaintext" but "distinguish this
  permutation of a known small domain from a random one", which is a different and easier
  question than the phrase "no known pairs" suggests.

Nothing in the ruling changes on this: the domain is 2³² wide, the viewer obtains no pairs,
and the consequence of a distinguisher would be entity-space recovery bounded by what C6 as
revised already accepts. But it is the setting the 8-round choice is actually made in, and it
belongs in the record.

### 3.1 The control-plane principal is OUTSIDE the defended set, and obtains chosen-plaintext pairs by construction

*(plan Important I-3. This is the premise the round-function ruling depends on, so it is
written down rather than left implied; a later reader must not have to reconstruct it.)*

Two mechanisms hand a control-plane caller exact `(entity_id, tessera_id)` pairs,
repeatably, with no attack involved:

1. `/status` returns `entity_id_high_water` — the allocator's next ID, in the clear.
2. `/control/ingest` returns the `tessera_id`s it just allocated, and the allocator is
   **monotone and dense** (I9), so the caller knows precisely which entity IDs those were.

Ingest a batch of *n* items and you hold *n* known plaintext/ciphertext pairs for the
deployment key — chosen, in the sense that you decide *n*.

**That is fine, and it is exactly why the round function does not need to be a
cryptographic PRF.** A control-plane principal already holds `/control/changes`, the ingest
path and the corpus size; entity IDs tell it nothing it cannot ask for directly. But it
means the correct statement of the defended property is **"a viewer-plane principal cannot
recover entity space"**, *not* "nobody can". An implementation that ever hands a
`tessera_id` and its entity ID to the same **viewer** would move the construction inside
the attacked set, where 8 rounds of a non-cryptographic mixer is not a claim this memo
makes. The byte-scanner (Task 13) and the layer check (Task 10) exist to stop that
happening by accident.

### 3.2 Consequences

- **No special key handling is required** beyond the bundle's existing protection. It is
  not a KMS key, it is not rotated on a schedule, it is not split.
- **But it must never leave the server, on any plane.** It appears in no API response
  (including `/meta`, `/status` and error bodies), in no log line, and in no metric label.
  Task 13 adds a conformance sweep for the key bytes on the viewer plane, exactly as the
  byte-scanner already sweeps for entity IDs.
- **Rotation is a breaking change for clients, not operational hygiene.** Do not schedule
  it. *(Amended 2026-07-30: since `tessera_id` became the storage sort key, rotation also
  **reorders tied rows** and therefore changes `permutation.bin` — see §6.)*
- **`priority`'s viewer-plane prohibition is RETIRED** *(amended 2026-07-30; the concurrent
  memo `docs/design-memos/2026-07-30-priority-as-identity-prefix.md` was adopted by owner
  decision, and this bullet as first written — a prohibition, plan Important I-4 — is
  **superseded**)*. The prohibition rested entirely on `priority` being an **unkeyed**
  `splitmix64` of the entity ID, which is what publishing would have made dangerous: a
  16-bit residue of the entity, a 65,536× narrowing of the search space per mark, computable
  offline against a candidate entity range and combinable across marks. `priority` is now
  `high16(tessera_id)` — 16 bits of a **keyed** identity the same payload already carries in
  **full** — so there is no unkeyed residue of the entity ID left to forbid. A viewer holding
  *k* marks learns only that their priorities fall below some cut *P*, and *P* is determined
  by *k* and the exact masked count §7.1 already returns. **The channel is closed by keying
  it, not by forbidding it**, and the prohibition is retired **by argument** rather than
  weakened by convenience: a future reader must not reinstate it without first
  re-establishing that `priority` is unkeyed.

  **The general rule** the prohibition was an instance of, and which `priority` now
  satisfies rather than escapes: *a hot column may be shown on the
  viewer plane only if it is independent of the entity ID, or keyed under the deployment
  key.*

  It is stated here unchanged because it is what **licenses** the redefinition rather than
  being collateral to it. Any future per-mark column derived from the entity ID by an
  *unkeyed* function inherits the prohibition `priority` has just been released from.

### 3.3 The construction against the invariants

Checked explicitly, because a construction that touches identity is exactly where an
invariant gets broken by a plausible-looking change:

- **I2** (every aggregate computable from inside `M_auth` alone) — **untouched**. The
  bijection is applied at build to produce a column value, and read at gather for output. It
  participates in no count, no density, no cluster and no summary; mask composition uses the
  *forward* permutation (`entity_to_row`) projected into row space and never sees a
  `tessera_id`. The epoch check on `/v1/items` is entity-independent by construction (§2a).
- **I4** — the identity is a value at a row, not a labelling or gating decision; nothing in
  §1 evaluates a visibility rule or moves a frontier.
- **I9** (entity IDs permanent, never reused, signature-sorted) — **untouched, and depended
  upon**. The bijection reads entity IDs; it does not assign them. The signature-sorted
  assignment stays exactly as §11.1 fixes it, and the allocator cap of §5a *narrows* the
  space rather than reusing any of it: exhaustion is refused, not wrapped.
- **I10** (entity IDs never cross the trust boundary) — **strengthened in substance while
  its mechanism changes**. Before: `columns.arrow` held the entity ID at every row, the
  gather read it, and `tessera-wire`'s per-session handle table was the last line of defence
  at the serialisation chokepoint — a runtime discipline. After: `columns.arrow` holds no
  entity ID at all, so **the gather cannot produce one**; the entity ID exists only in
  entity-space structures and as the *index* of `permutation.bin`. The `EntityId` grep in
  `scripts/check-layers.sh` still holds and is extended in Task 10. The two channels the
  retired handle closed as a side effect — existence-over-time probing and cross-principal
  correlation — are the intended trade of D5 and are registered as **C17**, not left silent.
- **I13** — the fail-closed obligations are met at every branch this memo introduces: an
  absent `identity` object, a degenerate or mis-cased key, disagreeing key sources, a
  differing construction or round count, a differing partitioning without an epoch bump, an
  entity above `u32::MAX`, allocator exhaustion, and a sidecar that cannot account for an
  entity — every one **refuses** rather than defaulting, truncating, wrapping or returning
  an empty answer.

---

## 4. Width — `u64`, and why narrowing is refused

Stay at `u64`, even though a single-shard Phase 1 deployment could encode the whole entity
space in 32 bits.

**Narrowing later is a breaking change for every client**, and the contract is open exactly
once. The 4 extra bytes buy forward compatibility with multi-shard, and the identity column
is **width-neutral against the `entity_id: uint64` it replaces**, so `u64` costs nothing
against today's bundle. On the wire it is 8 B/point against the retired per-session
handle's 4 B: **+0.58 MB on a 2.00 MB payload at k=1000**, which is an input to the
drawn-mark plan's `DEFAULT_MAX_K` calibration and is recorded there as such.

A 32-bit identity would also make the shard prefix unrepresentable, which would mean
either dropping the reserved field (§5) or re-widening at the moment sharding arrives — the
one moment at which a format change is most expensive.

---

## 5. Which prefix — the §13.3 row-range shard

`shard_id` means the **§13.3 row-range shard** (sharding for scale), **not the §12
partition** (compartmented isolation). §13.3 is explicit that row IDs are a spatial
ordering and shards are contiguous row ranges; §13.4 rules sharding premature below ~10⁸,
so **Phase 1 is single-shard and `shard_id = 0`**, recorded in MANIFEST.

**The partition-versus-shard ambiguity is resolved** *(review round 2)*, for three
independent reasons:

1. **Entity IDs are already bundle-global across partitions, in the code today.**
   `Manifest::entity_id_high_water` is a **single** field on the bundle manifest, not one
   per `PartitionDescriptor`, and `Engine` carries **one** `Allocator` seeded from it. Two
   items in different partitions cannot receive the same entity ID. `partitions/default/`
   is a *container* for postings, geometry and sidecars; it is not an ID namespace. A
   partition component in the bijection's input would therefore encode **nothing** — it
   would be a constant.
2. **A §12 partition could not be a 32-bit prefix even if one were wanted.** §12.4 fixes
   partition identity as *"a canonical hash of the sorted required set"*, because
   partitions are **discovered, not declared** — created on first sight of a new required
   set, converging between racing workers precisely *because* the identity is a content
   hash. A hash is not a dense small integer, and a dense small integer could not be
   assigned without the global coordination §12.3's isolation property exists to avoid.
   This is a structural fact about §12, not a Phase 1 simplification.
3. **§12 partitions exist in the format today; §13.3 row-range shards do not.**
   `partitions/<phash>/` is in the §2.1 layout tree and `partitions: Vec<PartitionDescriptor>`
   is in the manifest; there is no shard concept in the bundle at all.

So the prefix is a **reserved field, correctly valued 0**, whose justification is forward
compatibility with the axis that does not exist yet. That is also why Task 5's
`the_shard_prefix_separates_identity_spaces` test matters: it is the only thing keeping a
reserved-and-unused field from being silently dropped from the input encoding by a later
simplification.

**The narrowed residual, recorded as an open question and blocking nothing.** §16's
exhaustion entry proposes *"shard-local u32 with a (partition, shard, offset) global ID"*.
**Open: whether a future multi-shard deployment allocates entity IDs per shard rather than
globally.** If per shard, the prefix stops being reserved and becomes load-bearing, and the
32/32 split is exactly right; if globally, the prefix stays 0 forever and the 4 bytes buy
nothing but the option. **Either way the encoding is unchanged and nothing in Phase 1 turns
on it.** Task 4 states this resolution in §16 and does **not** decide the exhaustion
question.

---

## 5a. The preconditions the ruling is conditional on

**The ruling to keep `splitmix64` at 8 rounds (§8) is conditional on these fixes. A memo
that omits them misrepresents the review.**

1. **The allocator cap at `u32::MAX`.** `Allocator::allocate` is today `lo + n` on a `u64`
   with **no cap at all**, and `Allocator::new` seeds from a `u64` high-water.
   "Collision-free by construction" therefore currently rests on nothing but the corpus
   being small. **`allocate` must refuse to hand out any ID `≥ u32::MAX`** — a typed error
   (`AllocError::Exhausted { high_water }`), **not** a wrap and **not** a panic on a
   serving path — and `Allocator::new` must refuse a seed above the same bound. This is
   §16's entity-ID-exhaustion question arriving early, and refusing is the fail-closed
   answer: an ingest that would exhaust the space is rejected, **the WAL is untouched**,
   and the operator is told. (Task 7.)
2. **`forward`'s checked conversion** (§1.8), which is what makes a bypass of the cap loud
   rather than silent. The two must land together. (Task 5.)
3. **The control-plane threat-model statement** (§3.1), written down rather than implied,
   because the choice of a non-cryptographic mixer depends on that premise.
4. **The channel that would hand a viewer an unkeyed residue of the entity ID is closed**
   (§3.2) — the one that would otherwise move the construction inside the attacked set.
   **This precondition stands; only the mechanism satisfying it has changed** *(amended
   2026-07-30)*. As ruled, it was satisfied by **prohibiting** `priority` on the viewer
   plane. It is now satisfied by **keying** it: `priority` is `high16(tessera_id)`, so it is
   no longer a residue of the entity ID at all, and no unkeyed derivative of the entity ID
   is emitted on any plane. **The condition is met, not dropped** — the round-function
   ruling still rests on something real, and the general rule at §3.2 (a hot column may
   cross only if it is independent of the entity ID or keyed under the deployment key)
   remains the operative test for every future column. Recorded rather than deleted because
   this section says in terms that a memo omitting a condition misrepresents the review.

---

## 6. Determinism

The bijection is a **pure function of `(key, shard_id, entity_id)`**. Both build paths —
the streaming pipeline and the in-memory reference path — read the same key from the same
`BuildArgs`, apply the same function to the same entity IDs, and produce **byte-identical
`columns.arrow`**. **No seed is threaded, no RNG is constructed, and no ordering dependence
exists.** This is strictly simpler than the random-mint design it replaces, which is why
the earlier draft's seeded-minting machinery is deleted rather than adapted, and it is what
`build_equivalence.rs` asserts.

The only non-determinism anywhere near the construction is the **CSPRNG mint**, which
happens once, at an operator's explicit request, outside the build's data path.

**The sort order moves onto the identity** *(amended 2026-07-30; this replaces the "the
sort tiebreak does not move" statement first written here, which said `tessera_id` was "a
column value written at the row, not a sort key" and now **inverts**)*. Under
`priority = high16(tessera_id)` a function of `tessera_id` **is** the sort key, so the
identity must exist before the tiler runs. The statement below is normative and must not be
paraphrased:

> **The storage sort order is `(morton, tessera_id)` ascending, and it needs no further
> tiebreak.**
>
> `tessera_id` is a keyed bijection over 2⁶⁴ and there is exactly one row per entity, so
> within a build no two rows share a `tessera_id` and the order is **total**. Because
> `priority` is a *prefix* of `tessera_id`, "order by `(morton, priority, tessera_id)`" is
> **identically** "order by `(morton, tessera_id)`" — there is no composite comparator to
> get subtly wrong, and an implementation may compare the 16-bit prefix first purely as an
> optimisation, provided a test asserts the two agree. `entity_id` is **not** a sort key at
> any position. `priority` is **not** an independent sort key: it is a cached prefix, and it
> must be derived at exactly one place from the `tessera_id` it is a prefix of.
>
> **What the oracle must re-derive:** row order as
> `(morton_of(x, y, extent), forward(identity.key, identity.shard_id, entity_id))` ascending.
> **Row order is therefore key-dependent, where it previously was not** — the oracle already
> reads `identity` from MANIFEST, so this adds a dependency and no new artifact. A different
> key gives a different order within a Morton cell; that is intended, and it is not a defect
> for `build_equivalence.rs`, which passes one key to both build paths.
>
> **The one condition under which an explicit tiebreak returns:** if a future format ever
> stores more than one row per entity, `tessera_id` stops being unique per row and the order
> stops being total. Nothing in Phase 1 does this — `permutation.bin`'s `entity_to_row` is a
> bijection — but a Phase 3 or later change that breaks it must supply a tiebreak explicitly
> rather than inherit an unspecified one.

**The build sequence therefore inverts: `tessera_id` is computed BEFORE the sort, not
written at the row after it.** This is the consequence most likely to produce a wrong
implementation if left vague. The sequence, for **both** build paths:

1. Allocate entity IDs — signature-sorted assignment, §11.1, **unchanged**.
2. **`tessera_id = forward(identity.key, identity.shard_id, entity_id)` for every item —
   before the tiler.** Fallible (§1.8): a checked conversion, collected into a `Result`.
3. Morton codes from `x`, `y` against the extent — unchanged.
4. **Sort by `(morton, tessera_id)`**, permuting the companion `entity_ids` vector
   identically. That vector is still needed — for `permutation.bin`'s `entity_to_row` and
   for the external-ID extents and locator — but it is a **companion, not a sort key**,
   which is a different reason from the one first given here for passing it alongside
   `TilerItem`.
5. Derive the `priority` column as `(tessera_id >> 48) as u16` **over the already-sorted
   `tessera_id` vector**. It is a projection of a column already in hand, computed at exactly
   one place, never an independent hash.
6. Write `columns.arrow` as `(tessera_id, x, y, priority, …declared scalars)`.

**Key rotation's blast radius widens accordingly.** With a key-dependent sort key, a rotation
**reorders tied rows**, so `columns.arrow` row IDs are no longer invariant under it: a rebuild
under a new key produces a different `permutation.bin` as well as different identifiers. The
determinism claim above is untouched — the build is still a pure function of
`(key, shard_id, entity_id)` — but "rotation invalidates every outstanding identifier"
becomes "rotation invalidates every outstanding identifier **and changes row order**".
Rotation was already a breaking change and remains one; this widens what breaks, not how
often, and it must be stated wherever rotation is described (§2.1's `--rotate-id-key` row,
§3.2's rotation bullet).

`tessera_id` is nonetheless **stored** at the row rather than recomputed per gathered mark,
and the reason is unchanged by the inversion above: the gather is a tight zero-copy loop over
mmap'd columns, and eight `splitmix64` rounds per mark at ~143,857 marks per viewport is work
the format can pay for once at build. The build sequence adds a second reason — the value must
exist before the sort in any case, so storing it costs nothing the build had not already
spent. It is nonetheless **derivable**, which is what lets `tessera verify` check the whole
column against the key.

---

## 7. What remains in sidecars

Two directions, both off the viewport path, both per-*interaction* or per-*admin-call*
under the routing principle:

| direction | caller | cadence | structure |
|---|---|---|---|
| `entity → external_id` | `/v1/items` drill-down (D4, D6) | per click | **one** positional `u32` locator into the sorted external-ID extents (`entities/ext-locator.u32`, singular — no `<k>` suffix) |
| `external_id → entity` | `/control/ingest` dedup, `/control/changes` past WAL retention | per admin call | the existing sorted `external-ids-<k>.arrow` family, entity narrowed to `uint32` |

**There is no `tessera_id → entity` sidecar.** Inversion is a pure function; it needs no
file, no map and no I/O. This is the single largest simplification the bijection buys.

**(a) The sidecar holds rows only for items whose caller supplied a key** *(owner Ruling
A)*. It is a **translation table between two representations of one identity**, not a store
of identities. An item with no caller key has **nothing to translate**: its identity *is*
its `tessera_id`, it occupies **no extent row** — only a `0xFFFFFFFF` locator slot, which
is the **ordinary** case rather than a missing value. A deployment whose callers supply no
keys at all pays **zero** sidecar disk and still has a complete, stable, global identifier
for every item. **No code path may manufacture an external ID for an item that has none.**

**(b) The whole structure is a PLACEHOLDER** *(owner Ruling B)*. It stands in for a future
adopted metadata store: keep it **minimal**, keep **both directions behind one module
surface**, and mark it as transitional in the spec (Task 4) and at the type (Task 8).
Appendix D rejects adopting an external store for the **access-control layer**; it does
**not** forbid adoption for a **cold store that never participates in masking**, which is
what this is. Any replacement inherits the same conditions, and they are the acceptance
criteria for it:

- **Off the request path.** It never participates in mask composition, in tile counts, or
  in any aggregate. §10.3's uncompressed-mmap rule protects request paths; this store sits
  on none.
- **Fail-closed.** Unavailable, inconsistent or short → a typed error, never a `None`
  dressed as "this item has no external ID". Specifically: live in-memory map first
  (authority for everything ingested since the build), then the bundle locator
  (`0xFFFFFFFF` → genuinely no external ID), and an entity past the locator's length that
  is at or below the allocator high-water and absent from the live map is an
  **inconsistency** — `Err(StoreError::InvalidSidecar { .. })`, not `Ok(None)`.
- **No authorisation decision depends on it**, and nothing it returns can widen a mask.

---

## 8. The honest limitation, and the ruling already taken

**`splitmix64` is not a cryptographic PRF.** This is a **blinding permutation**, and its
security rests entirely on the *viewer* never obtaining known `(entity_id, tessera_id)`
pairs — a premise §3.1 states, and which holds for the viewer plane and explicitly does
**not** hold for the control plane. Eight rounds of a non-cryptographic mixer should not be
assumed to resist an adversary who does obtain such pairs.

**Review round 2 ruled: keep `splitmix64` at 8 rounds**, conditional on the four fixes in
§5a. The construction was independently verified invertible in the same round (§1). **This
point is a record, not an open question.** A subsequent review should re-open it **only**
if it finds an error in that verification, and must say what the error is.

**The reasons for `splitmix64` over a cipher**, so the trade is visible rather than
implied:

- It is **already contract in this spec** (§2.6's `priority`), so the oracle already
  reproduces it and a second reader has **one** construction to learn instead of two.
- It is ~2 ns rather than ~150 ns for SHA-256, which at 8 rounds × 10⁹ items is ~16 s of
  build rather than ~20 min.
- The property being defended (§3) does not need a cipher.

**The trade against "design for audit before performance"**, stated rather than dodged:
the bijection buys **collision-freedom by construction** and a **pure function** — no
`tessera_id → entity` file, no map, no I/O, no ordering dependence — at the cost of an
obviously-correct sorted array that any reviewer could check by eye. What makes the trade
acceptable is that the whole construction is **~30 lines**, with a round-trip test, an
empirical bijection check (§1.9) and **fixed known-answer vectors shared by two independent
implementations**. Reviewability is bought back by the vectors rather than by the code
being trivial.

**Second choice, recorded:** keyed **SipHash-1-3** as the round function, at ~80–120 s
added to a ninety-minute build (~2%). It was not taken, but it is the drop-in if the threat
model ever changes — the Feistel structure and everything in §§2–7 are unaffected by
swapping `F`, and only the `construction` string in MANIFEST and the vectors file would
change.

**The fallback that was NOT taken, and why it is worse:** a **128-bit random ID**,
collision-free in practice and needing no crypto in the oracle at all — at **26 B/row ≈
25.0 GiB**, which is *worse than today's* `columns.arrow` and reopens the residency problem
this whole change exists to close. It also reintroduces the `tessera_id → entity` map the
bijection dissolves, and with it a non-deterministic build.

---

## 9. The known-answer vectors

`reference/vectors/tessera_id.json`. **Both** Task 5 (Rust) and Task 12 (Python) test
against this file; the disambiguation rule is at the head of this memo and is restated
inside the file itself.

Canonical test key `000102030405060708090a0b0c0d0e0f`
(`k0 = 0x0706050403020100`, `k1 = 0x0f0e0d0c0b0a0908`). Contents:

| block | count | what it catches |
|---|---|---|
| `splitmix64` | 6 | a wrong shift constant (30/27/31), a wrong golden-ratio or multiplier constant, signed shift, missing 64-bit wraparound — **before** the round loop is blamed |
| `key_schedule` | `k0`, `k1`, 8 round keys | big-endian half read, swapped halves, `i` instead of `i + 1` |
| `vectors` | 33 | the construction end to end: `entity_id ∈ {0, 1, 2, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFE, 0xFFFFFFFF}` × `shard_id ∈ {0, 1, 0xFFFFFFFF}`, plus 12 random entities across shards 0, 1, 7 and `0xFFFFFFFF`. **Every one is asserted in both directions.** |
| `inverse_only` | 6 | the inverse loop *independently* of the forward one — arbitrary `u64`s (including `0`, `u64::MAX`, and ids whose halves are `0`/`0xFFFFFFFF`) and the `(shard, entity)` they invert to. An off-by-one round index in `invert` alone fails here. |
| `secondary_key` | 6 | an implementation that ignores the key, hard-codes the schedule, or reads the halves the wrong way round — **and a non-wrapping `k1 × (i + 1)`**, which only this block can catch *(corrected 2026-07-30, from the independent gate: the row above previously claimed the credit. It cannot. Under the canonical key `k1 = 0x0f0e0d0c0b0a0908`, and `k1 × 8` — the largest multiple the schedule takes — is `0x78706860585048 40`, which never overflows `u64`, so the primary `key_schedule` block exercises no wraparound in that multiply at all. The secondary key's `k1 = 0xefcdab8967452301` does overflow from `i + 1 = 2` upward, so a missing 64-bit reduction there changes every round key and every vector in this block. Wraparound inside `splitmix64`'s own multiplies is the `splitmix64` block's job and is a different bug.)* |
| `rejected_keys` | 6 | the all-zero key, `k1 == 0` with a non-zero `k0`, **uppercase hex**, 30 characters, 34 characters, a non-hex character — each with the reason it must be refused |

Which realistic failures each edge case covers: a **swapped half** shows up on every
`shard_id ≠ 0` vector and on `inverse_only`; an **off-by-one round index** shows up
everywhere in `vectors` and is localised by `inverse_only`; a **wrong shift** is localised
by `splitmix64`; **sign/width errors** show up on `0x7FFFFFFF`, `0x80000000` and
`0xFFFFFFFF`, which is why the entity set brackets the sign bit on both sides rather than
just taking `u32::MAX`.

The vectors were computed by a **throwaway Python script written from this memo's text**,
not by running Task 5's implementation, which does not exist. The script is not committed;
it is reproducible from §1 alone, which is the property being tested.

---

## Change list

| Where | Change |
|---|---|
| contracts §1 | external-ID cap 256 → 64 bytes; sidecar disk scales with key length |
| contracts §2.2 | the seven build flags, the N-1 refusal, `--id-key-file`, `identity_epoch` on `/meta`, optional `epoch` on `/v1/items` |
| contracts §2.4 | `identity` in MANIFEST; the sidecar as placeholder; compression as a conditional future option |
| contracts §2.6 | the construction (§1 verbatim); `columns.arrow`'s identity column; `priority` redefined as `high16(tessera_id)` and the sort order as `(morton, tessera_id)` *(amended 2026-07-30; this row previously read "`priority` forbidden on the viewer plane", which is retired — §3.2, §5a precondition 4, §6)* |
| design §10.6, I10 | mechanism clause: opaque `tessera_id` in place of per-session handle; substance unchanged |
| design Appendix C | C6 revised; **C17 added** (stable wire identity across sessions and principals) |
| design §16 | the partition-versus-shard resolution; the narrowed residual |
| `reference/vectors/tessera_id.json` | the shared gate (§9) |
