# Mosaica documentation

This directory is the source of the documentation site, configured by `mkdocs.yml` at the repository root. `bash scripts/check-docs.sh` builds it and runs the documentation checks. The TypeScript and components reference under `reference/typescript/` and `reference/components/` is generated from the client sources, so a plain `mkdocs build --strict` needs `node clients/ts/scripts/reference.mjs` run first. [index.md](index.md) is the site's home page.

- [start/](start/index.md): tutorials for a new user.
- [guides/](guides/index.md): the steps for particular tasks.
- [reference/](reference/index.md): the HTTP API, the clients, the command line and the declaration format.
- [system/](system/overview.md): how Mosaica works, in this reading order: [overview](system/overview.md), [data model](system/data-model.md), [access control](system/access-control.md), [security](system/security.md), [queries](system/queries.md), [annotations](system/annotations.md), [write path](system/write-path.md), [serving](system/serving.md), [clients](system/clients.md).
- [developer/](developer/index.md): building, testing and changing Mosaica.
- [openapi/](openapi/): the HTTP contract, rendered into the reference.

[writing.md](writing.md) is the style for prose. It and this file are not part of the site. Working notes are in [notes/](../notes/).
