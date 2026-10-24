# Mosaica

Mosaica is a database for interactive maps of very large sets of points. You give it records that each have a position. They might be places with a longitude and latitude, or documents laid out by an embedding model. Mosaica serves a map of them that people can pan, zoom, filter and search, and it works out the counts, densities and cluster labels afresh for whatever is on screen.

It is designed to hold billions of points on one machine and to serve them to many viewers at once. The data can change while it serves. New records appear on the map at the next flush, which runs every 90 seconds by default. A deletion applies to every request that starts after the server accepts it.

Mosaica can also give each viewer a map of their own. Every record can carry an access label, and each viewer holds a set of permissions. A viewer's map is then computed only from the records they may see, including every count and cluster. A record hidden from someone leaves no trace on their map.

## Ways to use it

Mosaica is one program, `mosaica`, which builds a database from your files and serves it over HTTP. You can work with it from four places. The HTTP API and the two clients can each declare data, insert, delete and query it. The command line builds and serves, and reads records and counts. It cannot delete or draw a map yet.

- **Python.** Build a database from a pandas or polars data frame, draw it as a map in a Jupyter notebook, and query it from code. See the [Python client](reference/python/index.md).
- **The command line.** Describe your Parquet files in a declaration, build them, and serve the result to browsers. [Your first map from files](start/first-map.md) walks through it.
- **The HTTP API.** Read and write a running server from any language. See the [HTTP API reference](reference/http.md).
- **TypeScript.** Put the map and its controls in a web page as web components, or as a layer in your own deck.gl scene. See the [TypeScript client](reference/typescript.md) and the [components](reference/components.md).

## About these pages

- [Start](start/index.md) has tutorials, which take you from nothing to a working map.
- [Guides](guides/index.md) cover particular jobs, such as running Mosaica under Docker or behind TLS.
- [Reference](reference/index.md) lists every route, command, setting and client call.
- [How it works](system/overview.md) explains the design: how data is stored, how a map is answered, how changes reach a running server, and how each viewer's map is kept to what they may see.
