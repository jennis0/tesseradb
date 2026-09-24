# Python client

The `tesseradb` package reads a Tessera database over HTTP, maps it in a notebook, and makes a database in a directory from data frames and files. Every table it returns is a pyarrow table. The `widget` extra, `tesseradb[widget]`, adds the notebook map.

`Map`, `create`, `open` and `Database` are loaded when first used, so `import tesseradb` works without anywidget or pyarrow.

These are the names `tesseradb` exports:

| Name | What it is | Page |
|---|---|---|
| `connect` | A reader of a database someone else runs | [Connecting and creating](connecting.md) |
| `create`, `open` | A new database in a directory, or one saved there | [Connecting and creating](connecting.md) |
| `Database` | A database in a directory, with its own server | [Connecting and creating](connecting.md), with its methods on the page for what they do |
| `Token`, `authorise`, `revoke` | A token, and how an operator makes and ends one | [Connecting and creating](connecting.md) |
| `Refusal` | The exception for a request that was refused | [Connecting and creating](connecting.md) |
| `Viewer`, `Selection`, `Sample` | A reader, part of a view, and the points a map draws | [Reading](reading.md) |
| `Map` | The notebook map | [The notebook map](widget.md) |
| `__version__` | The package's version, as a string | |

The package's calls return these types too. They are not imported by name.

| Type | What it is | Page |
|---|---|---|
| `Declared`, `DeclaredColumn` | What `Database.declare_columns` declared | [Declaring](declaring.md) |
| `Insert` | What `Database.insert` handed over | [Inserting and committing](inserting.md) |
| `Report`, `CommitReport`, `PagedReport` | What `Database.check` and `Database.commit` found or did | [Inserting and committing](inserting.md) |
| `ChangeReport` | What a deletion, a suppression or `Database.leave` did | [Operating](operating.md) |
| `Listening`, `Control`, `Answer` | The server's addresses, and a client for its control plane | [Operating](operating.md) |

The methods of `Database` are on four pages: [declaring](declaring.md), [inserting and committing](inserting.md), [reading](reading.md) and [operating](operating.md). The rest are on [connecting and creating](connecting.md).
