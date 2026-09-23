# Tessera documentation

This directory is the source of the documentation site, built by `mkdocs.yml` at the repository root with `mkdocs build --strict`; `bash scripts/check-docs.sh` builds it and runs the documentation checks. [index.md](index.md) is the site's home page.

- [start/](start/index.md): tutorials for a new user.
- [guides/](guides/index.md): the steps for particular tasks.
- [reference/](reference/index.md): the HTTP API, the clients, the command line and the declaration format.
- [system/](system/overview.md): how Tessera works, in this reading order: [overview](system/overview.md), [data model](system/data-model.md), [access control](system/access-control.md), [security](system/security.md), [queries](system/queries.md), [annotations](system/annotations.md), [write path](system/write-path.md), [serving](system/serving.md), [clients](system/clients.md).
- [developer/](developer/index.md): building, testing and changing Tessera.
- [openapi/](openapi/): the HTTP contract, rendered into the reference.

[writing.md](writing.md) is the style for prose. It, this file, [roadmap.md](roadmap.md), [outstanding.md](outstanding.md) and [ingest-campaign.md](ingest-campaign.md) are working notes and are not part of the site.
