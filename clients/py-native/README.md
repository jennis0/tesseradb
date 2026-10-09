# mosaica-native

The platform wheel behind [`mosaica`](https://pypi.org/project/mosaica/). It carries two
compiled artifacts and nothing else:

- `mosaica`, the server and CLI binary, at `mosaica_native.binary_path()`.
- `_mosaica`, the extension module that checks a declaration in process.

`mosaica` depends on this distribution under a platform marker, so `pip install mosaica` gets
both by default. An unsupported platform installs `mosaica` pure Python, and so does
`pip install mosaica --no-deps`.

The version is pinned exactly. A bundle format change refuses a stale bundle, so the binary and
the SDK move together.

Build it from a checkout of the repository, where `clients/py-native` sits beside the Rust
workspace; a cargo toolchain is the only thing needed beyond Python.
