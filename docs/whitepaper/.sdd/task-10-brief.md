### Task 10: Polish and publish

**Files:**
- Modify: `docs/whitepaper/src/00-style.html`, `docs/whitepaper/src/90-footer.html`

**Interfaces:**
- Produces: a published Artifact URL.

- [ ] **Step 1: Fill in the citations section**

`90-footer.html` gets the citation list assembled from the six fact sheets, each linking to its primary source where the prior-art documents give one.

- [ ] **Step 2: Word count check**

```bash
cd /home/joe/code/tessera && python3 -c "
import re,pathlib
h=pathlib.Path('docs/tessera-white-paper.html').read_text()
h=re.sub(r'<(script|style)[^>]*>.*?</\1>','',h,flags=re.S)
print(len(re.sub(r'<[^>]+>',' ',h).split()))"
```
Expected: 8,000–10,000. If materially under, the prose is thin — say so rather than padding.

- [ ] **Step 3: Accessibility and motion pass**

Confirm every interactive figure is keyboard-operable and focus-visible; every figure has a caption that states its point in prose, so the paper survives with images unavailable; `prefers-reduced-motion` is honoured by F13.

- [ ] **Step 4: Final full verification**

```bash
cd /home/joe/code/tessera
python3 docs/whitepaper/build.py && python3 docs/whitepaper/validate.py
/tmp/claude-1000/-home-joe-code-tessera/900913fe-e0d3-4bf1-a8dc-840014e64c28/scratchpad/wp-venv/bin/python docs/whitepaper/render_check.py
```
Expected: both print `PASS`. Read all four screenshots.

- [ ] **Step 5: Publish**

Call the Artifact tool with `file_path: docs/tessera-white-paper.html`, a `description` of one sentence, and `favicon: "🧩"`. Keep the favicon stable across any later redeploy.

- [ ] **Step 6: Report the URL and state plainly what was verified and what was not**
