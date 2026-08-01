# SDD ledger — plan: docs/superpowers/plans/2026-07-28-tessera-white-paper.md

Mode: no-git (owner decision, 2026-07-28). Ledger + file-based review packages
replace commits and diffs. Checkpoint = build + validate + render all passing.

Task 1: part-1 done (73 bullets, 1 uncited) — scale claims spot-checked OK
Task 1: part-3 done (63) — FLAG: "compartmented MAC" absent from corpus; source says
  "compartmented / lattice-based mandatory access control; dominance". FLAG:
  optimisations §2.3 holds non-DECIDED numbers confusable with the DECIDED
  signature-sort figures (§2.1/§4.4) — Task 5 must not conflate them.
Task 1: part-4 done (107, 0 uncited) — I2/I3/I7/I12 verbatim
Task 1: part-5 done (98, 2 uncited) — retirement-rule table verified verbatim vs lifecycle §3.1
Task 1: part-6 done (~45) — full §6 table + all 16 leak-register rows; memo hedges captured
Task 1: part-2 done (108, 3 uncited). FLAG: Roaring array/bitmap container
  threshold is NOT in the corpus — F5 must cite Roaring literature (Chambi et
  al./CRoaring) directly, not the design docs. Also absent: any worked
  tile-prefix -> row-range example; cold-start figure beyond "seconds".
Task 1: complete — 6 sheets, 509 cited bullets; all 15 uncited lines are
  absence-notes or sub-bullet headers, verified individually. Spot-checks:
  part-1 scale claims and part-5 retirement table verified against sources.
Task 2: implementer DONE. Controller verification: build OK (3 fragments,
  23883 B); validate = exactly 17 missing-figure failures, nothing else;
  render_check PASS across 1280/390 x light/dark; screenshots read, both
  themes correct.
Task 2: implementer concerns recorded — (a) cached chromium rev 1208 too old
  for playwright 1.61.0, `playwright install chromium` was run; (b) light-mode
  --cat-3/-4/-5 below 3:1 contrast, figures using those slots owe direct
  labels; (c) --band media queries load-bearing below 768px; (d) .fig-scroll
  inert around a bare svg, use .fig-fixed; (e) validator matches banned
  literals ANYWHERE incl. comments/prose.
Task 2: CONTROLLER FINDING — header eyebrow reads "ARCHITECTURE PAPER ·
  REVISION 18"; r18 is the architecture DESIGN DOC's revision, not this
  paper's. Misstatement if published. Enters fix loop.
Task 2: task reviewer dispatched.
Tasks 3-8: all six parts written and on disk; validator PASS (all 17 figures
  present), render PASS.
Controller fixes: (a) measure 34rem->43rem, band 6rem->11rem, added 68/58rem
  step-downs (user request: wider text and figures); (b) `.wp figure text`
  fill override beat every fill="" presentation attribute across the paper —
  now :not([fill]) so figures can opt out. Affected 25 elements in parts I/II.
Part II FINDING (verified independently by controller): the F4 premise in the
  plan was WRONG. Morton gives MORE runs than row-major for a free rectangle
  (8x8: 13.7 vs 8.0; 16x16: 28.4 vs 16.0). Morton's win is per ALIGNED
  quadtree tile (1 run vs s runs). Figure rebuilt on tiles. Plan text at
  Task 4 Step 4 is wrong and should not be reused.
Part VI FINDING: retroactive revocation is NOT open — resolved r17. The
  .ignore/README.md "Open" list is stale vs the design doc.
Word counts over guide: II 2356, IV 2801, V 2292 — all declined to cut
  sourced material. Accepted.

=== Review round (post-writing) ===
Invariant review: 1 Critical (Part I said tile = contiguous range of ENTITY ids;
  must be ROW ids — fixed by controller), 6 Important, 5 Minor. All 10 delegated
  findings now verified fixed against source.
Third-party review: 3 Critical. Root cause = F2b's axes (scale x granularity)
  cannot express the paper's claim. Elasticsearch (per-doc, 10^9 per Elastic's
  own DLS javadoc) and Accumulo (per-cell, "near-exact match for the per-document
  predicate") both belong in the supposedly empty corner. Three invented
  coordinates: Milvus 10^6 (from "10 roles/row", wrong quantity), deepscatter-Gaia
  10^9 (marketing tagline), PostgreSQL 10^7 (our own benchmark corpus size).
DECISION (owner): replace F2b scatter with a systems x 4-criteria capability
  matrix — row-level ACL / 10^9 identifiable / aggregates inside mask /
  interactive per-viewport repaint. Owner identified criterion 4 (realtime
  repaint) as the missing axis; it is now load-bearing.
Accumulo argument corrected: do NOT rest on 250 ms (contention-dependent,
  tunable to 5 ms; survey disclaims absolute numbers). Rest on the missing
  primitive — no exact masked count without visiting every cell, no visibility
  index in RFile, ~8-10 ms uncached auth setup per scan RPC. Work not wall-clock:
  ~240M cell visits for 3 panned scenes vs 3 x 0.31 ms on one box.
