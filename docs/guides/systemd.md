# Run Tessera under systemd

This guide turns the map you served from a terminal in [the first tutorial](../start/first-map.md)
into a service on a Linux server, which starts at boot and comes back by itself if it crashes.
You'll need sudo on the server and the `tessera` binary you built with cargo. From the tutorial's
`~/ireland` directory you need `corpus.toml`, `points.parquet` and `.env`, which holds the identity
key.

Once the service is running, [Operate a deployment](operating.md) covers what you do with it, from
who may reach each address to compaction. [Run Tessera with Docker](docker.md) does the same job as
this page in a container.

!!! note ""
    Not built yet: a package for any Linux distribution. Copy the binary into place by hand, as
    the first step shows.

## Install the binary and make a user

The service gets a user of its own. The bundle holds every item in the corpus, whatever its access
label, and the identity key with them, so no other account on the machine should be able to read it.

```bash
sudo useradd --system --home-dir /srv/tessera --shell /usr/sbin/nologin tessera
sudo install -m 0755 ~/.cargo/bin/tessera /usr/local/bin/tessera
```

## Lay out /srv/tessera

The service needs to write to two places only, the bundle and a `state` directory holding the
write-ahead log and the cache. Keeping them apart from the declaration in `corpus` and the
credentials in `secrets` lets systemd make everything else read-only to it. Only the `tessera`
user can open `secrets`.

```bash
sudo install -d -o tessera -g tessera -m 0750 /srv/tessera /srv/tessera/corpus /srv/tessera/state /srv/tessera/state/wal
sudo install -d -o tessera -g tessera -m 0700 /srv/tessera/secrets
sudo install -o tessera -g tessera -m 0640 ~/ireland/corpus.toml ~/ireland/points.parquet /srv/tessera/corpus/
sudo install -o tessera -g tessera -m 0600 ~/ireland/.env /srv/tessera/.env
```

The server creates its cache directory when it first starts. It does not create the directory the
log goes in, which is why `state/wal` is made here. Without it the server stops at once, with an
error that doesn't say which directory it wanted:

```text
tessera serve: refused to start: wal error: wal io error: No such file or directory (os error 2)
```

From here on the page uses two shells. A command after `$` runs in your own shell and uses `sudo`. A
command after `tessera$` runs as the `tessera` user in `/srv/tessera`, which is the only account
that can read the credentials. Open that shell like this:

```console
$ sudo -u tessera bash
tessera$ cd /srv/tessera
```

## Write tessera.toml

Save this as `/srv/tessera/tessera.toml`, as the `tessera` user:

```toml
[bundle]
path  = "bundle"
cache = "state/cache"
wal   = "state/wal/wal.log"

[build]
schema = "corpus/corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:9151"
session = "127.0.0.1:9152"
control = "unix:/run/tessera/control.sock"
session_credential_file  = "secrets/session.secret"
operator_credential_file = "secrets/operator.secret"
```

The paths under `[bundle]`, `schema` and both credential files are read relative to the directory
`tessera.toml` is in, wherever the server is started from.

In the tutorial the control address, which takes writes to the corpus, was a TCP port on
`127.0.0.1`. Any program on the machine can connect to that, and only the operator credential keeps
it out. Here it is a unix socket, a file only the `tessera` user can open, so the operating system
turns every other account away before the credential is even read. Write a socket's path in full,
since it is not read relative to `tessera.toml`.

The viewer and session addresses stay on `127.0.0.1`. [Addresses and
credentials](operating.md#addresses-and-credentials) says who should reach each one, and what
`token_max_lifetime` is for.

Every section of `tessera.toml` refuses a key it does not know and names the ones it takes. The keys
this guide leaves out keep their defaults.

!!! note ""
    Not built yet: a reference page for `tessera.toml`. Until there is one, each key is a field of
    `RawServe` or `RawIngest` in
    [`crates/tessera-config/src/lib.rs`](https://github.com/jennis0/tesseradb/blob/main/crates/tessera-config/src/lib.rs),
    with its default in
    [`defaults.rs`](https://github.com/jennis0/tesseradb/blob/main/crates/tessera-config/src/defaults.rs).

## Create the credentials

The server will not start without the session and operator credentials `tessera.toml` names. They
use the tutorial's file names:

```console
tessera$ umask 077
tessera$ openssl rand -hex 32 > secrets/session.secret
tessera$ openssl rand -hex 32 > secrets/operator.secret
```

## Build the bundle

`tessera build` finds the identity key in the `.env` you copied beside `tessera.toml`, as it did in
the tutorial. Run it as the `tessera` user, so the service owns the bundle and can write to it
later.

```console
tessera$ tessera build
...
built /srv/tessera/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

A later build is different, because the running server changes the bundle and a browser may hold a
`tessera_id` from it. [Rebuild and replace the bundle](rebuild.md) covers it.

## Start it by hand

Run the server once in the `tessera` shell before handing it to systemd. A mistake in `tessera.toml`
then shows up in front of you, not in the journal. systemd will make the socket's directory under
`/run` for you; for now, make it yourself.

```console
$ sudo install -d -o tessera -g tessera -m 0700 /run/tessera
```

```console
tessera$ tessera serve
2026-09-24T09:26:04.709794Z  INFO tessera_server::memory: the allocator's arena count is capped arenas=12
2026-09-24T09:26:04.727664Z  INFO tessera_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-24T09:26:04.727934Z  INFO tessera_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9151","session":"127.0.0.1:9152","control":"unix:/run/tessera/control.sock"}
```

If `/run/tessera` is missing, the server stops with an error that does not say which path it wanted:

```text
tessera serve: No such file or directory (os error 2)
```

In a second `tessera` shell, ask the viewer address whether the server is ready, and ask the session
address for a token the way your backend will. Under the passthrough plugin, `auth_data` is base64
of a JSON object whose `terms` list the access labels to grant.

```console
tessera$ curl -sSi http://127.0.0.1:9151/readyz
HTTP/1.1 200 OK
content-length: 0
date: Thu, 24 Sep 2026 09:26:04 GMT

tessera$ curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.secret)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}"
{"token":"2546b15027f3b33d8639941a5406597e1ee2bf60715d46dbfb5e4268ac0547d1","token_id":0,"expires_at":1790245564}
```

Press Ctrl-C in the first shell. The server prints `tessera serve: stopped on SIGINT` and exits.

## Hand it to systemd

Save this as `/etc/systemd/system/tessera.service`:

```ini
[Unit]
Description=Tessera
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=tessera
Group=tessera
WorkingDirectory=/srv/tessera
ExecStart=/usr/local/bin/tessera serve --deployment /srv/tessera/tessera.toml
Restart=on-failure
RestartSec=5
MemoryMax=24G
MemorySwapMax=0
UMask=0077
RuntimeDirectory=tessera
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=/srv/tessera/bundle /srv/tessera/state

[Install]
WantedBy=multi-user.target
```

`RuntimeDirectory` makes `/run/tessera` each time the service starts and removes it when it stops.
`UMask=0077` means the socket, and every file the server writes, can be opened by the `tessera` user
alone.

`MemoryMax` caps the service's memory, and 24G is a placeholder; [Memory](operating.md#memory)
explains how to choose the figure. `MemorySwapMax=0` keeps the service out of swap, so it meets the
cap rather than being paged out.

`Restart=on-failure` restarts the server when it exits with an error or is killed, and at no other
time, because a server that has lost the ability to write its log stays up and reports itself not
ready. Restarting it then can bring back an item someone suppressed, as [When a write to the log
fails](operating.md#when-a-write-to-the-log-fails) explains. Don't add an automatic restart on
readiness.

The last five settings confine the service. `NoNewPrivileges` stops the server and anything it
runs from gaining privileges. `PrivateTmp` gives it a `/tmp` of its own, and `ProtectHome` hides
every home directory from it. `ProtectSystem=strict` makes the rest of the filesystem read-only to
it, apart from the runtime directory and the paths in `ReadWritePaths`. If you move the bundle or
the log elsewhere, change `ReadWritePaths` to match.

Start the service, and have it start at boot:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now tessera
journalctl -u tessera -f
```

The journal shows the same lines you saw when you started it by hand. The server logs at `INFO` and
above, and `RUST_LOG` does not change that. `systemctl stop tessera` sends SIGTERM, which stops the
server at once; [Stopping and restarting](operating.md#stopping-and-restarting) says what that
means for requests in progress.
