# Why unrelated code moves the scan's constants: it is instruction-address alignment, and nothing else

**Date:** 2026-08-11 · **Status:** Evidence, not normative · **Machine:** WSL2 on Linux 6.18,
AMD Ryzen 9 5900X (Zen 3), 47 GB RAM · **Harness:**
[`probes/2026-08-08-filter-layout/layoutprobe`](../../../probes/2026-08-08-filter-layout/layoutprobe)
`realscan`, 10⁸, interleaved A/B, medians of three to five, pinned to one core.

## Results

**The mechanism is code-layout alignment, measured and isolated.** The 30–70% swings arm 16
recorded are caused by the hot loops' *addresses* moving relative to 64-byte boundaries — not by
the emitted machine code changing, not by inlining, and not by measurement error. The evidence
that discriminates it:

1. Reintroducing arm 14's reproducer (the fold's pass compiled inside `tessera-filter`, never
   called) reproduced the regression exactly — universal arms 0.25 → 0.42 ns per candidate entity
   (+65%) — and disassembly of both binaries shows the hot functions **instruction-for-instruction
   identical**. Every differing byte is a RIP-relative displacement; the whole block of scan code
   had simply moved by 0x50 bytes.
2. **A pure shift with no code change reproduces the full effect, with period 64.** Padding
   `tessera-filter`'s text with `global_asm!(".space N")` — sixteen inert bytes, nothing else —
   moves the constant between exactly two values depending on the shift mod 64:

   | shift of the hot functions (mod 64) | universal 1% contiguous | universal 25% broad |
   |---|---|---|
   | 0 (baseline) | 0.25 ns | 0.26 ns |
   | +16 | **0.42** | **0.43** |
   | +32 | **0.42** | **0.43** |
   | +48 | 0.26 | 0.25 |
   | +64 (≡ 0) | 0.27 | 0.26 |
   | +80 (≡ 16) | **0.42** | **0.43** |
   | +128 (≡ 0) | 0.28 | 0.27 |

   The +80 build places `scan_eq` at the same address as the arm-14 reproducer and times
   identically to it. The scattered arm (memory-latency bound) is unaffected throughout, exactly
   as arm 16 found.
3. **Pinning function starts to 64 bytes removes the sensitivity at no baseline cost.** Built with
   `RUSTFLAGS="-C llvm-args=-align-all-functions=6"`, baseline and perturbed trees measure
   0.25–0.26 ns — the *fast* value — and the padded build's hot functions land at identical
   addresses, so the channel by which unrelated code reaches the constant is closed. This is the
   remedy `codegen-units = 1` was not: that knob re-rolls the layout (hence "fixes one path, not
   the other") where this one pins it.

**§2.2's published constants are one draw from a bimodal distribution, and the fast one.** Over
the shifts measured, the universal-presence scan constant is ~0.25–0.28 ns or ~0.42–0.44 ns and
nothing in between — two of the four 16-byte residues land on each. ~0.27 ns is the scan's cost
*at a lucky layout*; an unlucky link of byte-identical scan code costs 65% more. Any consumer of
the constant should read it as "0.25–0.44 ns depending on layout, controllable to the low value".

**A scan-only crate would not fix this, and the caller's suspicion is confirmed.** A crate
boundary changes which object files the linker places where — it re-rolls the dice, it does not
load them. That is why arm 16 measured it "recovering about half": the new layout happened to
land partway. It would reduce how often the dice are re-rolled (fewer edits shift a crate that
never changes), which has real value for measurement hygiene, but the constant would remain a
property of link order, dependency versions and linker version. Alignment pinning is the fix;
crate isolation is at most a complement.

## What was measured, and how

Baseline **A** is the committed tree (`b818a00`); its `realscan` binary rebuilds byte-identically.
Perturbation **C** is `tessera-filter-write`'s `lib.rs` compiled into `tessera-filter` as a
private, never-referenced module — arm 14's shape. All runs are `realscan 100000000`, A/B
interleaved in strict alternation on a pinned core, medians reported. Run-to-run drift on the
universal arms was 2–5% quiet; the scattered arm 10.1–13.4 ns across the session, quoted as a
range per the campaign's own advice.

| arm (universal presence) | A | C | Δ |
|---|---|---|---|
| 1% contiguous | 0.25 ns | 0.41 ns | +64% |
| 25% broad | 0.25 ns | 0.42 ns | +68% |
| 1% scattered | 10.1–10.5 ns | 10.2–10.7 ns | within drift |

The per-candidate constants at 10⁸ match the 10⁹ figures the design quotes, so the effect was not
re-confirmed at 10⁹; nothing in the mechanism is scale-dependent, since the codegen is the same
binary either way.

**Where the two builds differ, exhaustively.** Same function set, same instruction sequences, same
sizes. The differences: the CGU content hash in `.llvm.<hash>` local-symbol suffixes, the order of
three small functions (`pack::Sink::flush` and companions), and a uniform +0x50 displacement of
the scan block. The regression therefore cannot be an inlining or codegen-unit story — the code
the CPU executes is the same bytes in a different place. Hypothesis "the emitted code actually
differs" is **refuted for this reproducer**; arm 16's byte-identical-source case is now explained
rather than mysterious, because byte-identical *emitted code* is sufficient for the effect.

**The pad experiment.** A `global_asm!` block emitting an `axR` text section of N inert bytes was
appended to `values.rs` — nothing reachable, nothing referenced; the linker happens to place it
immediately before the `pack_run` monomorphisations, so the hot functions shift by N rounded up to
16. Seven pad sizes were built and all nine binaries interleaved in five rounds on a pinned core
(the table above). Timing is a pure function of shift mod 64; shift mod 4096 varies freely across
the same cells, which also refutes the page/huge-page hypothesis — nothing page-scale is involved,
and at these sizes (1.1 MB text) no THP threshold is crossed.

**The affected loop** is the packed-block path — `pack_run`'s SWAR packing loop, which both
universal arms spend their time in. The presence-path arms (slice-blocked) are 0.02–1.2 ms at 10⁸,
too small to discriminate over this session's noise floor; arm 16 measured their sensitivity at
10⁹ and its independence from the packing path's is consistent with this mechanism — two hot
loops at two addresses, each with its own alignment, which is also why single-knob remedies fixed
one and not the other. Not re-measured here.

**Why 64 bytes, and why only the compute-bound arms** — mechanism at the microarchitectural level
is *attributed, not measured*: `perf` is not available under this WSL2 kernel, so cycle-level
attribution (op-cache hit rate, front-end stalls) could not be taken. The measured signature —
period 64, two discrete levels, tight compute-bound loops only, memory-bound arm immune — is the
classic front-end fetch/op-cache alignment effect on Zen 3, where a loop body's placement across
64-byte fetch lines changes decode bandwidth (cf. Mytkowicz et al., ASPLOS 2009, on layout
swamping real effects). The design does not depend on which front-end structure it is; it depends
on the effect being address-driven, which is measured.

## The remedy

`-C llvm-args=-align-all-functions=6` on release builds of anything linking `tessera-filter`:

- Padded and unpadded trees measure identically (0.25/0.26 vs 0.26/0.26 ns), at the **fast**
  constant — measured, five interleaved rounds.
- Baseline cost: none measurable on any arm, against `codegen-units = 1`'s measured ~15%. Binary
  grows 0.14% (1,568 bytes on `realscan`).
- What it guarantees: intra-function offsets become invariant mod 64, so *unrelated* additions to
  the crate — the whole class of arm 16 regressions — can no longer move a hot loop's alignment.
  What it does not: an edit to the hot functions themselves still relocates their own blocks, and
  layout luck within the function still applies. A change to `values.rs` or `pack.rs` still needs
  the interleaved A/B discipline; a change to anything else stops needing it.
- Caveat: measured on `realscan` only. Before relying on it, the flag wants applying to the
  workspace profile and the arm re-run against `tessera-server`'s actual binary, since the property
  claimed is layout invariance of *that* link. Not done here — it is a Cargo profile decision the
  owner should take with the design edit, not a probe-tree experiment.

An alternative worth knowing about and **not** taken: `-Z min-function-alignment` is nightly, and
per-function `#[align]`/`#[repr(align)]` on functions is not stable Rust; the `llvm-args` route is
the stable spelling of the same thing.

## Consequences for the corpus (not edited here)

- §2.2 should quote the constant as layout-conditioned (~0.25–0.28 ns *with alignment pinned*, or
  explicitly as the 0.25/0.42 pair without it), and arm 16's warning can now name the mechanism
  instead of the symptom.
- §6.2's "codegen units are partitioned per crate" paragraph attributes the fix to the wrong
  mechanism: the crate boundary worked by re-rolling layout, and the "within drift" row of arm
  14's table is luck, not structure. The `tessera-filter-write` split may stand on its
  audit-separation merits, but its performance rationale is refuted.
- The handover's §1 recommendation of a scan-only crate as "the structural answer" should be
  downgraded in favour of alignment pinning, for the reasons above.
- The text-predicate constants (1.7–96 ns) were not exercised by this session's reproducers;
  their compute-bound cells (contiguous `eq`/`prefix`/`in`, region-search `contains`) are the same
  kind of tight loop and should be assumed layout-sensitive until measured otherwise. The
  memory-latency-bound cells (scattered text) should be immune, as scattered fixed-width was.

## Independent check, and what it could not settle

A second run attempted to reproduce the decisive experiment from scratch — baseline, baseline plus
an inert `global_asm!(".space 16")`, and both again under the alignment flag. **It confirmed the
remedy's mechanism and settled nothing about the timings**, and both halves are worth recording.

**Confirmed, and it needs no timing at all.** Without the flag the four hot symbols sit at 64-byte
residues **16, 0, 48, 0** — varying, and two of them unaligned. Built with
`-C llvm-args=-align-all-functions=6` all four sit at residue **0**. The flag does mechanically
exactly what is claimed of it, which is checkable with `nm` and arithmetic rather than a stopwatch.

**Not confirmed, for two identified reasons, neither of which bears on the finding.** First, the
`.space 16` appended to `values.rs` was placed by the linker *after* the hot functions rather than
before them, so the perturbed binary's symbols were at byte-identical addresses to the baseline's —
there was no perturbation to measure, and the "A vs B" comparison was a binary against itself.
Second and more seriously, the machine was at load average 12 with a headless Chromium GPU process
taking ~780% CPU and a 23 GB `tessera serve` resident, and under that load **the same binary
measured 0.28, 0.46 and 0.49 ns on three consecutive rounds** — a 1.75× spread with the code held
fixed. No 65% effect is resolvable against that.

The practical lesson is the one the environment note below already gives, sharpened: on this
machine the interleaved A/B discipline is what makes a number mean anything, and a *quiet* machine
is a precondition even so. It is also why the period-64 pattern in the results above carries the
weight it does — noise does not produce a clean function of shift mod 64 in which +48 is fast,
+16 and +32 are slow, and +80 lands on the reproducer's own address and time.

## Environment notes, for whoever measures next

- `perf` is unavailable (no binary; WSL2 kernel without PMU passthrough). Plan measurements that
  do not need counters.
- Mid-session, a concurrent `cargo test --workspace` and a 23 GB resident `tessera serve` inflated
  *every* cell 2–3× — the baseline itself read 0.45–0.69 ns. Interleaving catches this (A and B
  degrade together); comparing against a number taken an hour earlier would have manufactured a
  phantom regression. The discipline in the handover is not optional on this machine.
- All experimental edits to `crates/tessera-filter` were reverted and the tree rebuilds
  byte-identical to `b818a00`'s binary; `git status` at finish shows this memo as the only change
  from this work (one unrelated pre-existing modification to
  docs/evidence/memos/2026-08-10-filter-rulings.md belongs to a concurrent session and was not
  touched).
