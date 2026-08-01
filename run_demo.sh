#!/bin/bash


# sed -i 's|bench-fixtures/2m4|bench-fixtures/1e9|' clients/ts/dev-server.toml
# rm -rf clients/ts/.dev/cache          # the fragment cache is per-bundle

# export TESSERA_SESSION_CRED=dev-session-credential
# export TESSERA_OPERATOR_CRED=dev-operator-credential
# ./target/release/tessera serve -c clients/ts/dev-server.toml &

cd clients/ts
TESSERA_SESSION_CRED=dev-session-credential node scripts/measure-principals.mjs --terms 0..200
~26 s, and it prints the table it wrote: 1,366 / 21,006 / 42,025,228 / 518,502,081.

npm run dev -w @tessera/viewer     # http://localhost:5173