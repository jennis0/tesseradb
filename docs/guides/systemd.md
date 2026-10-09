# Run Mosaica under systemd

This guide turns the map you served from a terminal in [the first tutorial](../start/first-map.md)
into a service on a Linux server, which starts at boot and comes back by itself if it crashes.
You'll need sudo on the server and the `mosaica` binary you built with cargo. From the tutorial's
`~/ireland` directory you need `corpus.toml` and `points.parquet`.

Once the service is running, [Operate a deployment](operating.md) covers what you do with it, from
who may reach each address to compaction. [Run Mosaica with Docker](docker.md) does the same job as
this page in a container.

!!! note ""
    Not built yet: a package for any Linux distribution. Copy the binary into place by hand, as
    the first step shows.

## Install the binary and make a user

The service gets a user of its own. The bundle holds every item in the corpus, whatever its access
label, so no other account on the machine should be able to read it.

```bash
sudo useradd --system --home-dir /srv/mosaica --shell /usr/sbin/nologin mosaica
sudo install -m 0755 ~/.cargo/bin/mosaica /usr/local/bin/mosaica
```

## Lay out /srv/mosaica

The service needs to write to two places only, the bundle and a `state` directory holding the
write-ahead log and the cache. Keeping them apart from the declaration in `corpus` and the
credentials in `secrets` lets systemd make everything else read-only to it. Only the `mosaica`
user can open `secrets`.

```bash
sudo install -d -o mosaica -g mosaica -m 0750 /srv/mosaica /srv/mosaica/corpus /srv/mosaica/state /srv/mosaica/state/wal
sudo install -d -o mosaica -g mosaica -m 0700 /srv/mosaica/secrets
sudo install -o mosaica -g mosaica -m 0640 ~/ireland/corpus.toml ~/ireland/points.parquet /srv/mosaica/corpus/
```

The server creates its cache directory when it first starts. It does not create the directory the
log goes in, which is why `state/wal` is made here. Without it the server stops at once, with an
error that doesn't say which directory it wanted:

```text
mosaica serve: refused to start: wal error: wal io error: No such file or directory (os error 2)
```

From here on the page uses two shells. A command after `$` runs in your own shell and uses `sudo`. A
command after `mosaica$` runs as the `mosaica` user in `/srv/mosaica`, which is the only account
that can read the credentials. Open that shell like this:

```console
$ sudo -u mosaica bash
mosaica$ cd /srv/mosaica
```

## Write mosaica.toml

Save this as `/srv/mosaica/mosaica.toml`, as the `mosaica` user:

```toml
[bundle]
path  = "bundle"
cache = "state/cache"
wal   = "state/wal/wal.log"

[build]
schema = "corpus/corpus.toml"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:9151"
session = "127.0.0.1:9152"
control = "unix:/run/mosaica/control.sock"
operator_credential_file = "secrets/operator.secret"

[catalogue]
dir = "state/catalogue"
```

The paths under `[bundle]`, `schema`, the credential file and the catalogue's directory are read
relative to the directory `mosaica.toml` is in, wherever the server is started from. The catalogue
holds the deployment's users, groups, API keys and grants. The server creates it on its first
start, readable by the `mosaica` user alone.

In the tutorial the control address, which takes writes to the corpus, was a TCP port on
`127.0.0.1`. Any program on the machine can connect to that, and only the operator credential keeps
it out. Here it is a unix socket, a file only the `mosaica` user can open, so the operating system
turns every other account away before the credential is even read. Write a socket's path in full,
since it is not read relative to `mosaica.toml`.

The viewer and session addresses stay on `127.0.0.1`. [Addresses and
credentials](operating.md#addresses-and-credentials) says who should reach each one, and what
`token_max_lifetime` is for.

Every section of `mosaica.toml` refuses a key it does not know and names the ones it takes. The keys
this guide leaves out keep their defaults.

!!! note ""
    Not built yet: a reference page for `mosaica.toml`. Until there is one, each key is a field of
    `RawServe` or `RawIngest` in
    [`crates/mosaica-config/src/lib.rs`](https://github.com/jennis0/mosaica/blob/main/crates/mosaica-config/src/lib.rs),
    with its default in
    [`defaults.rs`](https://github.com/jennis0/mosaica/blob/main/crates/mosaica-config/src/defaults.rs).

## Create the credential

The server will not start without the operator credential `mosaica.toml` names. It uses the
tutorial's file name:

```console
mosaica$ umask 077
mosaica$ openssl rand -hex 32 > secrets/operator.secret
```

## Build the bundle

Run `mosaica build` as the `mosaica` user, so the service owns the bundle and can write to it
later.

```console
mosaica$ mosaica build
...
built /srv/mosaica/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

A later build is different, because the running server changes the bundle and a browser may hold a
`tessera_id` from it. [Rebuild and replace the bundle](rebuild.md) covers it.

## Start it by hand

Run the server once in the `mosaica` shell before handing it to systemd. A mistake in `mosaica.toml`
then shows up in front of you, not in the journal. systemd will make the socket's directory under
`/run` for you; for now, make it yourself.

```console
$ sudo install -d -o mosaica -g mosaica -m 0700 /run/mosaica
```

```console
mosaica$ mosaica serve
2026-09-24T09:26:04.709794Z  INFO mosaica_server::memory: the allocator's arena count is capped arenas=12
2026-09-24T09:26:04.727664Z  INFO mosaica_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-24T09:26:04.727934Z  INFO mosaica_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9151","session":"127.0.0.1:9152","control":"unix:/run/mosaica/control.sock"}
```

If `/run/mosaica` is missing, the server stops with an error that does not say which path it wanted:

```text
mosaica serve: No such file or directory (os error 2)
```

In a second `mosaica` shell, ask the viewer address whether the server is ready, and ask the session
address for a token. Your backend will ask with an API key whose principal holds `authorise-as`,
naming the principal to read as. The operator credential can instead name the access terms the
token holds, which needs no principal in the catalogue:

```console
mosaica$ curl -sSi http://127.0.0.1:9151/readyz
HTTP/1.1 200 OK
content-length: 0
date: Thu, 24 Sep 2026 09:26:04 GMT

mosaica$ curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/operator.secret)" \
  -H 'content-type: application/json' \
  -d '{"terms": ["public"]}'
{"token":"2546b15027f3b33d8639941a5406597e1ee2bf60715d46dbfb5e4268ac0547d1","token_id":0,"expires_at":1790245564}
```

Press Ctrl-C in the first shell. The server prints `mosaica serve: stopped on SIGINT` and exits.

## Hand it to systemd

Save this as `/etc/systemd/system/mosaica.service`:

```ini
[Unit]
Description=Mosaica
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=mosaica
Group=mosaica
WorkingDirectory=/srv/mosaica
ExecStart=/usr/local/bin/mosaica serve --deployment /srv/mosaica/mosaica.toml
Restart=on-failure
RestartSec=5
MemoryMax=24G
MemorySwapMax=0
UMask=0077
RuntimeDirectory=mosaica
NoNewPrivileges=yes
PrivateTmp=yes
ProtectHome=yes
ProtectSystem=strict
ReadWritePaths=/srv/mosaica/bundle /srv/mosaica/state

[Install]
WantedBy=multi-user.target
```

`RuntimeDirectory` makes `/run/mosaica` each time the service starts and removes it when it stops.
`UMask=0077` means the socket, and every file the server writes, can be opened by the `mosaica` user
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
sudo systemctl enable --now mosaica
journalctl -u mosaica -f
```

The journal shows the same lines you saw when you started it by hand. The server logs at `INFO` and
above, and `RUST_LOG` does not change that. `systemctl stop mosaica` sends SIGTERM, which stops the
server at once; [Stopping and restarting](operating.md#stopping-and-restarting) says what that
means for requests in progress.
