# Run Mosaica with Docker

This guide runs Mosaica in a container managed by Docker Compose, with the database kept in a Docker
volume. It suits a host where you already run services this way. You'll need Docker with the Compose
plugin, sudo on the host, and a clone of the Mosaica repository to build the image from. From the
`~/ireland` directory of [the first tutorial](../start/first-map.md) you need `corpus.toml` and
`points.parquet`.

Once the container is running, [Operate a deployment](operating.md) takes over. It applies to this
install and to [the systemd one](systemd.md) alike.

## Build the image

!!! note ""
    Not built yet: a published image. Build it from the repository.

The Dockerfile at the root of the repository compiles `mosaica` and copies it onto a distroless base
image, which holds the C library and certificates and nothing else. With no shell in the image,
every command you run in the container is a `mosaica` command.

```console
$ cd ~/mosaica
$ docker build --build-arg MOSAICA_BUILD_COMMIT=$(git rev-parse HEAD) -t mosaica .
...
#16 unpacking to docker.io/library/mosaica:latest
#16 unpacking to docker.io/library/mosaica:latest 0.2s done
#16 DONE 1.2s
$ docker run --rm mosaica --version
mosaica 101b9768ff3cda26f8e960572cd898c181fc9fd5
```

The build argument puts the commit into `--version`, which is how you tell two images apart later.

The image runs `mosaica serve` unless you give it another command. It runs as user 65532, not as
root, and looks for `mosaica.toml` in `/etc/mosaica`.

## Make a deployment directory

Compose works from one directory on the host, which holds `compose.yaml`, the configuration and the
credentials. The database itself lives in a volume that Docker manages.

```console
$ mkdir -p ~/mosaica-docker/config ~/mosaica-docker/secrets
$ cd ~/mosaica-docker
$ cp ~/ireland/corpus.toml ~/ireland/points.parquet config/
```

The container sees these at fixed paths.

| In the container | What it holds | Mounted from |
|---|---|---|
| `/etc/mosaica` | `mosaica.toml`, the declaration and the Parquet files it names | `config/`, read-only |
| `/var/lib/mosaica` | the bundle, the write-ahead log, the cache and the catalogue | the volume |
| `/run/secrets/operator.secret` | the operator credential | a file from `secrets/`, read-only |

## Write mosaica.toml

Save this as `config/mosaica.toml`:

```toml
[bundle]
path  = "/var/lib/mosaica/bundle"
cache = "/var/lib/mosaica/cache"
wal   = "/var/lib/mosaica/wal.log"

[build]
schema = "corpus.toml"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "0.0.0.0:8080"
session = "0.0.0.0:8081"
control = "0.0.0.0:8082"
operator_credential_file = "/run/secrets/operator.secret"

[catalogue]
dir = "/var/lib/mosaica/catalogue"
```

The bundle, the log, the cache and the catalogue of users, API keys and grants all go into the
volume, the only place in the container the server can write. `token_max_lifetime` is how long a browser's token lasts, as [Addresses and
credentials](operating.md#addresses-and-credentials) explains.

The three addresses listen on every interface inside the container. Other containers on the same
Docker network can reach them there, and so can any process on the host, through the container's
address on Docker's bridge network. Publishing a port in `compose.yaml` decides what else can reach
it. The operator credential is what keeps everyone who can reach the control port from changing
the corpus. Every address serves plain HTTP, and the server accepts a password sent to the viewer
address over it, so anything that reaches the viewer address from off the host goes through a TLS
proxy.

## Write compose.yaml

Save this as `compose.yaml`:

```yaml
services:
  mosaica:
    image: mosaica
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    volumes:
      - ./config:/etc/mosaica:ro
      - mosaica-data:/var/lib/mosaica
    secrets:
      - operator.secret
    ports:
      - "127.0.0.1:9161:8080"
      - "127.0.0.1:9162:8081"
      - "127.0.0.1:9163:8082"
    restart: unless-stopped

volumes:
  mosaica-data:

secrets:
  operator.secret:
    file: ./secrets/operator.secret
```

The ports are published to the host's loopback address only, so nothing off the machine reaches
Mosaica directly. Browsers reach the viewer address through a TLS proxy on the host, as [Put
Mosaica behind TLS](tls.md) describes. The repository's `docker/compose.yaml` publishes the viewer
and session ports on every interface instead, which suits a machine behind a firewall or a load
balancer. Publish the control port to loopback or not at all.

`read_only` mounts the container's own filesystem read-only, so the server can write to the volume
and nowhere else. `cap_drop: [ALL]` removes every Linux capability, none of which the server uses,
and `no-new-privileges` stops any process in the container from gaining privileges it started
without.

`restart: unless-stopped` starts the container again if the server exits, and after the host
reboots. It does not restart a container because it reports itself unhealthy. For Mosaica that is
the behaviour you want, for the reason [When a write to the log
fails](operating.md#when-a-write-to-the-log-fails) gives.

## Create the credential

The server needs the operator credential [Addresses and
credentials](operating.md#addresses-and-credentials) describes before it will start. Compose mounts
the file into the container with the owner and mode it has on the host, and the server runs as
user 65532. Make the file readable by you and by group 65532, and by no one else:

```console
$ chmod 700 secrets
$ openssl rand -hex 32 > secrets/operator.secret
$ chmod 640 secrets/operator.secret
$ sudo chgrp 65532 secrets/operator.secret
```

You can still read the file yourself, which the `curl` examples below rely on. Compose's longer
secret syntax has `uid`, `gid` and `mode` settings that would do this without `sudo`, but with a
secret read from a file Compose ignores them:

```text
time="2026-09-24T10:19:51+01:00" level=warning msg="secrets `uid`, `gid` and `mode` are not supported, they will be ignored"
```

If the server can't read the credential, it stops, and the restart policy starts it again, over and
over. The log shows why each time:

```text
mosaica serve: refused to start: cannot read the operator credential file /run/secrets/operator.secret (Permission denied (os error 13)); name a readable file, relative to mosaica.toml's directory or absolute
```

## Build the bundle

A container starts from an empty volume, so build the bundle into it first. `docker compose run`
starts a one-off container with the same mounts and runs the command you give it:

```console
$ docker compose run --rm mosaica build
...
built /var/lib/mosaica/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

## Start the server

```console
$ docker compose up -d
 Container mosaica-docker-mosaica-1 Creating 
 Container mosaica-docker-mosaica-1 Created 
 Container mosaica-docker-mosaica-1 Starting 
 Container mosaica-docker-mosaica-1 Started 
$ docker compose logs
mosaica-1  | 2026-09-24T09:26:52.648054Z  INFO mosaica_server::memory: the allocator's arena count is capped arenas=12
mosaica-1  | 2026-09-24T09:26:52.669887Z  INFO mosaica_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
mosaica-1  | 2026-09-24T09:26:52.670888Z  INFO mosaica_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
mosaica-1  | {"event":"listening","viewer":"0.0.0.0:8080","session":"0.0.0.0:8081","control":"0.0.0.0:8082"}
```

The last line means all three addresses are open. The addresses it names are the ones inside the
container, and from the host you use the published ports, 9161 to 9163.

Docker checks the container's health by running `mosaica health` inside it, which asks the viewer
address whether the server is ready. It checks every 2 seconds for up to ten minutes while the
server starts, since a large bundle can take minutes to open, and every 15 seconds after that.
`docker compose ps` shows the result:

```console
$ docker compose ps
NAME                       IMAGE     COMMAND                  SERVICE   CREATED         STATUS                   PORTS
mosaica-docker-mosaica-1   mosaica   "/usr/local/bin/tess…"   mosaica   7 seconds ago   Up 7 seconds (healthy)   127.0.0.1:9161->8080/tcp, 127.0.0.1:9162->8081/tcp, 127.0.0.1:9163->8082/tcp
```

From the host, ask for a token. Your application's backend will ask with an API key whose
principal holds `authorise-as`, naming the principal to read as. The operator credential can
instead name the access terms the token holds, which needs no principal in the catalogue:

```console
$ curl -sS http://127.0.0.1:9162/session/authorise \
  -H "authorization: Bearer $(cat secrets/operator.secret)" \
  -H 'content-type: application/json' \
  -d '{"terms": ["public"]}'
{"token":"37306e2d578bcf30b6685f8d4230224f2fbe304a1e140c0f6a7a57f29c813a08","token_id":0,"expires_at":1790245619}
```

The control address answers on port 9163:

```console
$ curl -sS http://127.0.0.1:9163/control/status \
  -H "authorization: Bearer $(cat secrets/operator.secret)" | jq .write_executor.posture
"running"
```

## Stop, remove and upgrade

`docker compose stop` sends the server SIGTERM, and it exits at once:

```console
$ docker compose stop
 Container mosaica-docker-mosaica-1 Stopping 
 Container mosaica-docker-mosaica-1 Stopped 
$ docker compose logs --tail 2
mosaica-1  | {"event":"listening","viewer":"0.0.0.0:8080","session":"0.0.0.0:8081","control":"0.0.0.0:8082"}
mosaica-1  | mosaica serve: stopped on SIGTERM
```

[Stopping and restarting](operating.md#stopping-and-restarting) says what that means for requests
in progress. `docker compose down` removes the container and keeps the volume. `docker compose down
-v` deletes the volume too, and with it the database and every change made to it since the build.

To upgrade, build a new image from the updated repository and start the service again. Compose sees
that the image has changed, replaces the container and keeps the volume.

```bash
cd ~/mosaica && git pull
docker build --build-arg MOSAICA_BUILD_COMMIT=$(git rev-parse HEAD) -t mosaica .
cd ~/mosaica-docker && docker compose up -d
```

A new version may not read a bundle an older one built. When the bundle's format differs, the server
refuses to start and says the bundle must be recreated. Rebuild it from the sources as the next
section shows, then send again any changes made while the old bundle was served.

## Replace the bundle

[Rebuild and replace the bundle](rebuild.md) explains why a rebuild gives every item a new
`tessera_id` and why the old log goes aside with the old bundle. Under Compose, copy the changed
sources into `config/`, here the Parquet file with Saint Patrick's Bridge corrected, and build into
a new directory in the volume.

The image has no shell, so moving directories in the volume needs a short-lived container from
another image. This uses Alpine Linux. Compose names the volume after the directory,
`mosaica-docker_mosaica-data`.

```console
$ cp ~/ireland/points.parquet config/
$ docker compose run --rm mosaica build --out /var/lib/mosaica/bundle.next 2>&1 | tail -2
built /var/lib/mosaica/bundle.next (v00000): 29935 items, 1 terms, 29935 pairs, 2181449 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
$ docker compose stop
 Container mosaica-docker-mosaica-1 Stopping 
 Container mosaica-docker-mosaica-1 Stopped 
$ docker run --rm -v mosaica-docker_mosaica-data:/data alpine sh -c 'cd /data && mkdir old && mv bundle cache wal-0* old/ && mv bundle.next bundle && ls'
bundle
old
$ docker compose start
 Container mosaica-docker-mosaica-1 Starting 
 Container mosaica-docker-mosaica-1 Started 
$ curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9161/readyz
200
```

`wal-0*` matches the log's files, which are named after the `wal` path in `mosaica.toml`. Once the
new bundle serves what you expect, delete `old` the same way:

```console
$ docker run --rm -v mosaica-docker_mosaica-data:/data alpine rm -rf /data/old
```
