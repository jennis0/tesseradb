### Task 9: Independent accuracy and invariant review

The paper makes public claims about third-party systems and about measured performance. This task exists because misstating I2 or I7 publicly is the worst available failure mode.

**Files:**
- Modify: whichever `docs/whitepaper/src/*.html` fragments the review identifies

**Interfaces:**
- Consumes: the built `docs/tessera-white-paper.html`.
- Produces: a review report, and fixes applied to the source fragments.

- [ ] **Step 1: Dispatch three reviewers concurrently**

Give each the built HTML path and no stake in the paper being right.

Reviewer A — invariants: "Read `.ignore/tessera-architecture-design.md` §4 and `.ignore/tessera-concurrency-lifecycle.md` §3. Then read the built paper. Report every place where a statement about I2, I3, I7, I9, I10, I12 or the three retirement rules differs in substance from the source, and every place the paper implies a guarantee the design does not make. Quote both the paper and the source."

Reviewer B — third-party claims: "Read `.ignore/prior-art-*.md`. For every claim the paper makes about a system other than Tessera, verify it against those documents and report any claim that is unsupported, overstated, or collapses the distinction between demonstrated and marketed capability. Report claims with no traceable source as failures."

Reviewer C — measurements: "Read `probes/results.md` and `probes/phase0-memo.md`. Verify every number in the paper against them, including units and measurement conditions. Report any number that is wrong, rounded misleadingly, quoted without its conditions, or presented as more general than the source supports. Confirm the synthetic-policy caveat is stated plainly rather than buried."

- [ ] **Step 2: Triage the findings yourself**

Do not accept reviewer summaries at face value. For each finding, read the cited source section directly and decide. Invariant-bearing decisions stay with you, not the reviewer.

- [ ] **Step 3: Apply the fixes to the source fragments, not the build output**

- [ ] **Step 4: Rebuild and re-verify**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```

- [ ] **Step 5: Checkpoint**
