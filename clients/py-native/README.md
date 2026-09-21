# tesseradb-native

The platform wheel behind [`tesseradb`](https://pypi.org/project/tesseradb/). It carries two
compiled artifacts and nothing else:

- `tessera`, the server and CLI binary, at `tesseradb_native.binary_path()`.
- `_tessera`, the extension module that checks a declaration in process.

`tesseradb` depends on this distribution under a platform marker, so `pip install tesseradb` gets
both by default. An unsupported platform installs `tesseradb` pure Python, and so does
`pip install tesseradb --no-deps`.

The version is pinned exactly. A bundle format change refuses a stale bundle, so the binary and
the SDK move together.

Build it from a checkout of the repository, where `clients/py-native` sits beside the Rust
workspace; a cargo toolchain is the only thing needed beyond Python.
