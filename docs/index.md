# Tessera

Tessera serves interactive maps over a corpus of documents or records, with access control at the level of the individual item. Each viewer's map is computed from exactly the items they are permitted to see: every count, density, cluster, label and sample, as well as the points themselves. One machine serves it, and the corpus can change while it serves: new items are ingested into the running service, and a deletion or suppression applies to every request from the moment it is accepted.

A corpus is declared once, with one or more two-dimensional layouts over it, such as geographic coordinates or an embedding projection. Tessera builds it into a bundle and serves it over an HTTP API. A TypeScript client with web components, a Python client with a notebook widget, and a command-line tool are built on that API.

- [Start](start/index.md) holds the tutorials, which take a new user from nothing to a working map.
- [Guides](guides/index.md) hold the steps for particular tasks.
- [Reference](reference/index.md) holds the HTTP API, the clients' interfaces, the command line and the declaration format, and a table of which surface does what.
- [How it works](system/overview.md) describes the system: its data model, access control, security argument, queries, annotations, write path, serving and clients.
- [Developer](developer/index.md) covers building, testing and changing Tessera itself.
