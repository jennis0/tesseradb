# Retiring `utf8` measured: `contains` gets 1.5–71× slower, everything else gets faster, and the crossover holds

**Date:** 2026-08-13 · **Status:** Evidence, not normative · **Machine:** WSL2 on Linux 6.18,
AMD Ryzen 9 5900X (Zen 3, 12 cores, 32 MiB L3), 47 GB RAM, single-threaded · **Harness:**
`utf8_retirement_fence.rs` (deleted with the baseline it measured — see the closing note) ·
**Raw:** [`probes/2026-08-13-utf8-retirement-fence/`](../../../probes/2026-08-13-utf8-retirement-fence/)

`records-and-search.md` §4.3 retires the flat `utf8` column and names the prices to be paid
knowingly, the third being "the `contains` constant-profile shift". Both formats are in the tree at
this moment and only at this moment. This is the fence: the shipped flat scan against the keyword
family's routes, on the same three real arXiv columns, the same candidates, the same needles, one
variable. Every keyword route was asserted to return the flat scan's exact bitmap before anything
was timed.

## Results

**1. `contains` is slower under the keyword family on every shape measured — 1.5× to 71×, and the
worst case is the interactive one.** Best keyword route against the flat scan, 25% candidate at
2.4M (*measured*): `id` contiguous 0.55 ms → 39.03 ms (**71×**), `submitter` contiguous 0.50 →
14.11 (**28×**), `doi` contiguous 6.68 → 23.91 (3.6×), and the three scattered candidates 1.5–4.9×.
The flat scan wins in every cell of both scales, including every point of the crossover sweep.

The ratio moves so much because the two costs have different shapes. The flat scan is linear in the
candidate — 0.91 ns per candidate entity contiguous, 14.5 ns scattered — and a contiguous run is
where `scan_text_contains` searches concatenated bytes as one region at SIMD throughput. The broad
keyword route is **flat in the candidate and linear in the vocabulary**: the dictionary walk is
essentially the whole cost. So the swap costs most where the candidate is small, which is the
per-keystroke cell, and least where it is the whole corpus and scattered.

At 10⁹ with a unique vocabulary, whole-corpus contiguous candidate (*modelled*, ×417 from 2.4M):
**flat ~1.1 s, keyword broad ~16 s, keyword narrow ~68 s.** §6.4 already carries this cell as one
that misses its budget and parallelises instead; what is new is that the column being retired
answered it single-threaded in about a second.

**2. `eq`, `prefix` and `in` improve or tie — and the `eq` gain depends entirely on which shipped
scan the route calls.** Contiguous 25% candidate, ns per scanned entity (*measured*): `eq` 2.237 →
0.275 on `id` (8.1×) and 1.172 → 0.207 on `submitter` (5.7×); `in` over eight needles 8.837 → 1.654
(5.3×) and 4.442 → 1.636 (2.7×); `prefix` 2.563 → 0.617 on `id` (4.2×). Scattered candidates
compress every ratio to 1.0–1.5×, because the cost there is bitmap traversal and one cache line per
entity, which both formats pay.

The entry point is worth an epic's attention. On `id` and `submitter`, `ValueColumn::scan_num_eq`
reaches the traversal through a one-element `binary_search` and measures **0.59–0.83 ns**, where
`ValueColumn::scan_eq` reaches the same traversal through a direct compare and measures
**0.21–0.28** — filter-index §2.2's own fixed-width constant, a 2.1–4.0× difference. On the sparse
`doi` the choice is immaterial (20.09 against 19.60): presence addressing dominates both.

Through `scan_num_eq` the keyword route is *slower than the flat scan on an absent needle* — 0.583
against 0.411 on `id`, because a needle whose length no stored value matches is rejected by the
flat scan on its length alone. Through `scan_eq` that regression disappears.

**3. On a sparse column the format barely matters.** `doi`'s every operator lands within 1.5× either
way, because a rank-addressed column's cost is the presence-run merge — about 9 ns per candidate
entity — whatever is stored. The swap neither gains nor loses there.

**4. `prefix` on a repeat-heavy column is not reliably better, and the reason is unresolved.**
Keyword `prefix` on `submitter` measures 0.695 ns per entity at 600k and 2.672 at 2.4M, while the
flat scan over the same column and candidate is stable (2.537, 2.708). That is a 3.6× win at one
scale and level at the other. The campaign rules out the format (the same column's `eq` is 0.21–0.22
at both scales) and build layout (both builds agree), and establishes no mechanism. **NOT confirmed:
that keyword `prefix` beats the flat scan on a repeat-heavy column — do not claim it.** On `id` the
4.2× advantage is solid at both scales.

**5. §4.3's 0.15 `contains` crossover holds, and it is conservative by up to 1.6×.** Measured by
sweeping the candidate's cardinality across the swap, as present candidate entities ÷ dictionary
size: **0.155, 0.165, 0.193, 0.199, 0.237** across five shapes, plus ~0.26 extrapolated for the one
a stride-4 candidate cannot reach. The rule sits at the bottom of that range, so on every shape it
switches to the broad route at or before the costs actually cross — it never picks the more
expensive route. The price of switching early is up to **20%** (`id` contiguous at 0.20: 40.01 ms
broad against 33.45 narrow), and erring that way is the safer direction, since the broad route's
cost is bounded by the vocabulary while the narrow route's grows without limit in the candidate.

**6. The dictionary walk with the substring search costs 10.7–25.1 ns per key**, flat across a 4×
corpus range and varying with the *needle's length* rather than with what it matches. The dictionary
campaign's 11.0–18.8 ns/key is decode alone; the gap is the search it omitted. At 10⁹ keys that is
**11–25 s single-threaded** (*modelled*).

**7. Assembling the broad route from `scan_num_in` costs it a further 64%.** Its "O(log k) per slot"
is priced for the eight values a caller types; broad `contains` hands it 22,500 matching ordinals.
At 2.4M, whole-corpus contiguous candidate on `id`: **63.88 ms through `scan_num_in` against 38.86
ms through a dense bitset over the dictionary's ordinals** (*measured*; the bitset is bench-local,
not shipped).

**8. Stored bytes: 2.16–3.56× smaller, confirmed on disk.** Through the shipped writers at 2.4M,
per present entity, presence counted on both sides: `id` 17.88 → 8.28 (2.16×), `submitter` 22.40 →
6.30 (3.56×), `doi` 33.57 → 11.01 (3.05×). This reaches the dictionary campaign's 2.18 / 3.61 /
3.13 by a different route — on-disk files with Arrow framing rather than an in-memory accounting —
and agrees to within 2%. Stable at 600k. It is the one unambiguous win in this campaign.

## What `records-and-search.md` owes, if the owner agrees

The design is frozen to this track; these are stated as owed, not made.

- **§4.3's third retirement price now has a number.** "The `contains` constant-profile shift" is
  measured at **1.5–71× slower than the column it replaces**, worst on a contiguous candidate,
  best on a scattered whole-corpus one. A regression that is understood is payable; this one is now
  understood.
- **§6.4's broad-`contains` row**: "11–19 s decode alone at 10⁹" should become **11–25 s for the
  walk including the search**, and the row can gain the flat column's counterpart — ~1.1 s
  single-threaded — as the baseline it is being measured against, while that baseline still exists
  to quote.
- **§4.3's crossover** can be re-marked from *modelled* to **measured 0.155–0.237, rule retained at
  0.15 as the conservative end**, with the denominator made explicit (stop-and-report A).
- **§4.3's narrow-route probe**, modelled at 0.1–0.3 µs, measures **0.07–0.15 µs** per candidate
  entity across the three columns.

## Stop-and-report

**A. Which cardinality the crossover compares is not stated, and on a sparse column it moves the
answer 2.2×.** §4.3 says "the candidate's cardinality against the dictionary's size". Against the
raw candidate the measured crossings are 0.193–0.368; against the candidate's *present* entities
they are 0.155–0.237. Both stay above 0.15 so the rule is safe either way, but the present-entity
denominator is the tighter one — and reading it means a route rule consulting the candidate's
intersection with the column's presence, which is a quantity about the principal's own visible data
and therefore an §8.2 admissibility question. **Not ruled on here.**

**B. The broad `contains` route needs an ordinal-set test that is constant per slot.** Result 7 is
an engine matter (`tessera-filter`), outside this track's allowlist. Naming it here so the route is
not built out of `scan_num_in` by default.

**C. Keyword `eq` should be routed through `scan_eq`, not `scan_num_eq`** — 2.1–4.0× on a column
the scan walks, and it is the
difference between the family beating the flat scan on an absent needle and losing to it. Also an
engine matter.

**D. ⊘ `-C llvm-args=-align-all-functions=6` is not in the tree.** `filter-index.md` says it lives
"in `.cargo/config.toml`"; no such file exists in this worktree or the main tree, and `.cargo/` is
in `.git/info/exclude`, so a fresh checkout builds without it and the alignment channel that
document says is closed at the build is open. This campaign set it through `RUSTFLAGS`. The path is
outside this track's allowlist.

## What this does not measure

**Scale is the honest limit.** 2.4M real records is what the corpus has; there is no larger real
string column, and every 10⁹ figure here multiplies a constant measured over structures that fit in
32 MiB of L3, which a 10⁹-key dictionary does not. §11 item 4 still owes the out-of-cache walk. No
synthetic key set was invented for it: a fabricated vocabulary front-codes according to how it was
fabricated, which would bias the one constant that matters most.

Also absent: concurrency (§6.4's `÷ cores` verdict is untested), paging (both columns are read into
owned buffers, not the request path's `Access::Mapped`), the write side (§11 item 7, and §4.3's
*first* named retirement price — nothing here measures the sort, the front-code or the coalesce's
dictionary merge), and decision 0067's term postings, which serve `eq` and `in` and would change
result 2's picture entirely.
