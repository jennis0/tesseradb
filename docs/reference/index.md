# Reference

These pages state what each interface accepts and returns.

- [HTTP API](http.md): the viewer, session and control planes, from the OpenAPI description.
- [Wire framing](../openapi/README.md): the framed Arrow bodies of `POST /v1/viewport` and `POST /v1/items`, with decoders in Python and JavaScript.
- [Capabilities](capabilities.md): which route, function, method or command performs each core capability on each of the four surfaces.
- [CLI](cli.md): the `tessera` command and its subcommands.
- [tessera.toml](tessera-toml.md): the deployment file, which says where the bundle is and how the server listens and bounds its work.
- [corpus.toml](corpus-toml.md): the corpus declaration, which says what a build reads and what each view, vocabulary, attribute and layer is.
- [Python client](python/index.md): the `tesseradb` package.
- [TypeScript client](typescript.md): the `@tesseradb/client` package.
- [Components](components.md): the web components, their attributes, events and styling.
