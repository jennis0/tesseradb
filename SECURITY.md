# Reporting a vulnerability

Report a suspected vulnerability through GitHub's private vulnerability reporting, from the Security
tab of this repository. Do not open a public issue.

Tessera computes every count, density, cluster, label and sample over the records the requesting
viewer is permitted to see. [docs/system/security.md](docs/system/security.md) is the threat model
and states what the system claims. A report is most useful when it names the claim that is broken.

The defects the threat model covers:

- an aggregate computed over more than the viewer's visible set: a count, histogram, density,
  cluster or sample
- a label served to a viewer whose authorised set does not admit it
- an entity id or a term id reaching a client
- a deletion or a suppression that a later request does not observe
- a filter that reveals an item rather than hiding one

No version is supported. Nothing is deployed and there are no released artefacts. A format change
bumps the format version so that a stale bundle is refused, rather than being migrated. A fix lands
on `main`.
