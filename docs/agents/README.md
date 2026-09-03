# Working in this repo

This directory says how work is done here. For what the system is, start at [`../design/README.md`](../design/README.md).

| Directory | Standing |
|---|---|
| [`../design/`](../design/) | The specification. `architecture.md` wins a conflict; a document's `Status:` line says whether it is normative or provisional |
| [`../decisions/`](../decisions/) | Settled decisions. Read before reopening one |
| [`../evidence/`](../evidence/), [`../../probes/`](../../probes/) | Measurements and investigations. Not normative |
| [`../roadmap.md`](../roadmap.md) | Order of work. Not status |
| GitHub issues | Status. `../ingest-campaign.md` records the test corpora and their build figures |

Which document: how a request works, `architecture.md` §2.6; what may leak, §4 and Appendix C; bytes on the wire or disk, `contracts.md`; the write path and the two deny removal rules, `write-path.md`; what a client may assume, `client-interaction.md`; whether something was measured, `evidence/memos/` and `probes/`.

Most work needs no procedure: make the change, run the gate, say what you did. Use [`design-process.md`](design-process.md) when a document is about to become binding, and [`parallel-work.md`](parallel-work.md) when several large independent tracks run at once. [`writing.md`](writing.md) is the house style. [`epic-lifecycle.md`](epic-lifecycle.md) says how issues are used.
