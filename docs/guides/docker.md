# Run Tessera with Docker

This guide runs Tessera in a container managed by Docker Compose, with the database kept in a Docker
volume. It suits a host where you already run services this way. You'll need Docker with the Compose
plugin and a clone of the Tessera repository to build the image from. From the first tutorial's
`~/ireland` directory you need `corpus.toml`, `points.parquet` and `.env`, which holds the identity
key.

Once the container is running, [Operate a deployment](operating.md) takes over. It applies to this
install and to [the systemd one](systemd.md) alike.

## Build the image

!!! note ""
    Not built yet: a published image. Build it from the repository.

The Dockerfile at the root of the repository compiles `tessera` and copies it onto a distroless base
image, which holds the C library and certificates and nothing else. There is no shell and no package
manager in it, so you can't open a shell in the container. Every command you run in it is a
`tessera` command.

```console
$ cd ~/tesseradb
$ docker build --build-arg TESSERA_BUILD_COMMIT=$(git rev-parse HEAD) -t tessera .
...
#16 unpacking to docker.io/library/tessera:latest
#16 unpacking to docker.io/library/tessera:latest 0.3s done
#16 DONE 0.4s
$ docker run --rm tessera --version
tessera 44e8d34e11a30c8e317066889ad7cfd5d05cb154
```

The build argument puts the commit into `--version`, which is how you tell two images apart later.

The image runs `tessera serve` unless you give it another command. It runs as user 65532, not as
root, and looks for `tessera.toml` in `/etc/tessera`.

## Make a deployment directory

Compose works from one directory on the host, which holds `compose.yaml`, the configuration and the
credentials. The database itself lives in a volume that Docker manages.

```console
$ mkdir -p ~/tessera-docker/config ~/tessera-docker/secrets
$ cd ~/tessera-docker
$ cp ~/ireland/corpus.toml ~/ireland/points.parquet config/
```

The container sees these at fixed paths.

| In the container | What it holds | Mounted from |
|---|---|---|
| `/etc/tessera` | `tessera.toml`, the declaration and the Parquet files it names | `config/`, read-only |
| `/var/lib/tessera` | the bundle, the write-ahead log and the cache | the volume |
| `/run/secrets` | the session and operator credentials | `secrets/`, read-only |

## Write tessera.toml

Save this as `config/tessera.toml`:

```toml
[bundle]
path  = "/var/lib/tessera/bundle"
cache = "/var/lib/tessera/cache"
wal   = "/var/lib/tessera/wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "0.0.0.0:8080"
session = "0.0.0.0:8081"
control = "0.0.0.0:8082"
session_credential_file  = "/run/secrets/tessera_session"
operator_credential_file = "/run/secrets/tessera_operator"
```

The bundle, the log and the cache all go into the volume, the only place in the container the server
can write.

The three addresses listen on every interface inside the container. Only other containers on the
same Docker network can reach those directly. What the host and the outside world can reach is
decided in `compose.yaml`, by the ports it publishes. `token_max_lifetime` works as in the tutorial:
it is how many seconds a browser's token lasts.

## Write compose.yaml

Save this as `compose.yaml`:

```yaml
services:
  tessera:
    image: tessera
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    environment:
      TESSERA_IDENTITY_KEY: ${TESSERA_IDENTITY_KEY:-}
    volumes:
      - ./config:/etc/tessera:ro
      - tessera-data:/var/lib/tessera
    secrets:
      - tessera_session
      - tessera_operator
    ports:
      - "127.0.0.1:9161:8080"
      - "127.0.0.1:9162:8081"
      - "127.0.0.1:9163:8082"
    restart: unless-stopped

volumes:
  tessera-data:

secrets:
  tessera_session:
    file: ./secrets/tessera_session
  tessera_operator:
    file: ./secrets/tessera_operator
```

All three ports are published to the host's loopback address only, so nothing off the machine can
reach Tessera directly. The browser-facing viewer address goes out through a TLS proxy on the host,
as [Put nginx in front of the viewer address](tls.md)
describes. The repository's `docker/compose.yaml` publishes the viewer and session ports on every
interface instead, which suits a machine with a firewall or a load balancer in front of it. Publish
the control port to loopback, or not at all: it accepts deletions and new data from anyone holding
the operator credential.

`read_only`, `cap_drop` and `no-new-privileges` take away everything the server does not need. It
writes only to the volume.

`restart: unless-stopped` starts the container again if the server exits, and after the host
reboots. Docker does not restart a container because it reports itself unhealthy, and for Tessera
that is the behaviour you want; [When a write to the log
fails](operating.md#when-a-write-to-the-log-fails) explains why.

The `environment` line hands the identity key to the container, which `tessera build` needs and the
server does not. Compose reads the value from a `.env` file in this directory.

## Create the credentials

The server will not start without both credentials. The session credential lets your application's
backend ask for viewer tokens, and the operator credential lets whoever holds it change the corpus.

```console
$ chmod 700 secrets
$ openssl rand -hex 32 > secrets/tessera_session
$ openssl rand -hex 32 > secrets/tessera_operator
$ chmod 644 secrets/tessera_session secrets/tessera_operator
```

The files need to be readable by everyone because Compose mounts each one into the container with
its owner and mode unchanged, and the server runs as user 65532, which does not own them. The
`secrets` directory, readable only by you, keeps other accounts on the host out. If you leave the
files readable by you alone, the server cannot open them, and because of the restart policy Compose
starts it over and over:

```text
tessera serve: refused to start: cannot read the session credential file /run/secrets/tessera_session (Permission denied (os error 13)); name a readable file, relative to tessera.toml's directory or absolute
```

## Build the bundle

A container starts from an empty volume, so build the bundle into it first. `docker compose run`
starts a one-off container with the same mounts and runs the command you give it. Copy the
tutorial's `.env` in beside `compose.yaml`, so Compose can pass the key through.

```console
$ cp ~/ireland/.env .
$ docker compose run --rm tessera build
...
built /var/lib/tessera/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
```

Once the bundle is built, delete the copy; the original is still in `~/ireland`. The server reads
the key from the bundle, and while `.env` is here Compose hands the key to every container it
starts, where `docker inspect` shows it. You need the key again only to rebuild after losing the
volume, as [The identity key and rebuilds](rebuild.md) explains.

```console
$ rm .env
```

## Start the server

```console
$ docker compose up -d
 Container tessera-docker-tessera-1 Creating 
 Container tessera-docker-tessera-1 Created 
 Container tessera-docker-tessera-1 Starting 
 Container tessera-docker-tessera-1 Started 
$ docker compose logs
tessera-1  | 2026-09-24T09:02:53.356227Z  INFO tessera_server::memory: the allocator's arena count is capped arenas=12
tessera-1  | 2026-09-24T09:02:53.367637Z  INFO tessera_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
tessera-1  | 2026-09-24T09:02:53.367967Z  INFO tessera_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
tessera-1  | {"event":"listening","viewer":"0.0.0.0:8080","session":"0.0.0.0:8081","control":"0.0.0.0:8082"}
```

The last line means all three addresses are open. The addresses it names are the ones inside the
container; from the host you use the published ports, 9161 to 9163.

Docker checks the container's health every 15 seconds by running `tessera health` inside it. That
asks the viewer address whether the server is ready, and fails if it is not. `docker compose ps`
shows the result:

```console
$ docker compose ps
NAME                       IMAGE     COMMAND                  SERVICE   CREATED         STATUS                   PORTS
tessera-docker-tessera-1   tessera   "/usr/local/bin/tess…"   tessera   7 seconds ago   Up 7 seconds (healthy)   127.0.0.1:9161->8080/tcp, 127.0.0.1:9162->8081/tcp, 127.0.0.1:9163->8082/tcp
```

A large bundle can take minutes to open, so failed checks in the first ten minutes are not counted
against it.

From the host, ask for a token the way your application's backend will. Under the passthrough
plugin, `auth_data` is base64 of a JSON object whose `terms` list the access labels to grant.

```console
$ curl -sS http://127.0.0.1:9162/session/authorise \
  -H "authorization: Bearer $(cat secrets/tessera_session)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}"
{"token":"dc76dbe16bc34e13e80e041b9298ddb7e518226efb2228cd698ee70cb5123d6c","token_id":0,"expires_at":1790244180}
```

The control address answers on port 9163 with the operator credential. [Operate a
deployment](operating.md) uses it for the server's status and for compaction. Its examples use the
systemd layout's addresses; with Compose, send the same requests to port 9163 and read the
credential from `secrets/tessera_operator`:

```console
$ curl -sS http://127.0.0.1:9163/control/status \
  -H "authorization: Bearer $(cat secrets/tessera_operator)" | jq .write_executor.posture
"running"
```

## Stop, remove and upgrade

`docker compose stop` sends the server SIGTERM, and it exits at once:

```console
$ docker compose stop
 Container tessera-docker-tessera-1 Stopping 
 Container tessera-docker-tessera-1 Stopped 
$ docker compose logs --tail 2
tessera-1  | {"event":"listening","viewer":"0.0.0.0:8080","session":"0.0.0.0:8081","control":"0.0.0.0:8082"}
tessera-1  | tessera serve: stopped on SIGTERM
```

Nothing it acknowledged is lost, because every change is written to the log and flushed to disc
before the caller is told it succeeded. `docker compose down` removes the container and keeps the
volume. `docker compose down -v` deletes the volume too, and with it the database and every change
made to it since the build.

To upgrade, build a new image from the updated repository and start the service again. Compose sees
that the image has changed and replaces the container, keeping the volume.

```bash
cd ~/tesseradb && git pull
docker build --build-arg TESSERA_BUILD_COMMIT=$(git rev-parse HEAD) -t tessera .
cd ~/tessera-docker && docker compose up -d
```

A new version may not read a bundle an older one built. When the bundle's format differs, the server
refuses to start and says the bundle must be recreated. Rebuild it from the sources, as the next
section shows, and send again any changes made while the old one was served.

## Replace the bundle

[Replace the bundle with a rebuilt one](rebuild.md#replace-the-bundle-with-a-rebuilt-one) explains
why a rebuild goes into a new directory and why the old log and cache are moved aside. Under Compose
the steps are the same, with one difference. The image has no shell, so the move is done by a
short-lived container from another image, here Alpine Linux, with the volume mounted. Compose names
the volume after the directory, `tessera-docker_tessera-data`.

```console
$ docker compose run --rm tessera build --carry-id-key-from /var/lib/tessera/bundle --out /var/lib/tessera/bundle.next 2>&1 | tail -2
built /var/lib/tessera/bundle.next (v00000): 29935 items, 1 terms, 29935 pairs, 2181446 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s)
  view ireland: 29935 row(s)
$ docker compose stop
 Container tessera-docker-tessera-1 Stopping 
 Container tessera-docker-tessera-1 Stopped 
$ docker run --rm -v tessera-docker_tessera-data:/data alpine sh -c 'cd /data && mkdir old && mv bundle cache wal-0* old/ && mv bundle.next bundle && ls'
bundle
old
$ docker compose start
 Container tessera-docker-tessera-1 Starting 
 Container tessera-docker-tessera-1 Started 
$ curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9161/readyz
200
```

`wal-0*` matches the log's files, which are named after the `wal` path in `tessera.toml`. Once the
new bundle serves what you expect, delete `old` the same way:

```console
$ docker run --rm -v tessera-docker_tessera-data:/data alpine rm -rf /data/old
```
