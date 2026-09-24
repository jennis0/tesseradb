# Tessera in Docker

The image holds the `tessera` binary on a distroless Debian base: glibc and CA certificates, with no shell and no package manager. It runs as uid 65532, and the root filesystem can be mounted read-only. The server tunes glibc's allocator, so the base is glibc.

## Build the image

From the repository root:

```bash
docker build --build-arg TESSERA_BUILD_COMMIT=$(git rev-parse HEAD) -t tessera .
```

`tessera --version` prints the commit given here, or `unknown` without it. A first build takes about ten minutes. Later builds reuse the compiled dependencies through BuildKit's cache.

## Where things go

| Path in the container | What it holds | Mount |
|---|---|---|
| `/etc/tessera` | `tessera.toml`, `schema.toml` and the source files `schema.toml` names | read-only |
| `/var/lib/tessera` | the bundle, the write-ahead log and the cache | a named volume |
| `/run/secrets/tessera_session`, `/run/secrets/tessera_operator` | the session and operator credentials | read-only |

The working directory is `/etc/tessera`, so `tessera build` and `tessera serve` find `tessera.toml` there with no flags. [config/tessera.toml](config/tessera.toml) is a starting point: it puts the bundle, log and cache in `/var/lib/tessera`, binds the three listeners on every interface, and reads the credentials from `/run/secrets`. A source in `schema.toml` is named by a path relative to `schema.toml`, so the source files sit beside it.

| Port | Plane | Who reaches it |
|---|---|---|
| 8080 | viewer | browsers and clients, with a token |
| 8081 | session | the application that mints tokens, with the session credential |
| 8082 | control | the operator, with the operator credential; it takes every write |

Publishing a port to the host's loopback keeps it off the network. Other containers on the same Docker network still reach every port, including control, which still needs the operator credential.

## Run it with Compose

[compose.yaml](compose.yaml) runs the image read-only with every capability dropped, and publishes the control plane on the host's loopback only.

```bash
cd docker
cp /path/to/schema.toml /path/to/*.parquet config/
mkdir -p secrets
openssl rand -hex 32 > secrets/tessera_session
openssl rand -hex 32 > secrets/tessera_operator
echo "TESSERA_IDENTITY_KEY=$(openssl rand -hex 16)" > .env

docker compose run --rm tessera build
docker compose up -d
curl -s localhost:8080/readyz -o /dev/null -w '%{http_code}\n'
```

`build` creates the database in the volume. `up` serves it and restarts it unless stopped. A write sent to the control plane goes to the log in the volume and survives a restart. `docker compose down` stops the server and keeps the volume; `down -v` deletes the database.

The identity key is needed by `build` only. Keep it: a later rebuild of the same corpus needs the same key, or every `tessera_id` a client holds changes.

Compose mounts a file secret with the file's host owner and mode. The container's uid 65532 must be able to read it, so leave the files world-readable in a private directory, or `chown 65532` them.

## Run it with `docker run`

```bash
export TESSERA_IDENTITY_KEY=$(openssl rand -hex 16)   # keep it: a rebuild needs the same key
docker volume create tessera-data
docker run --rm -v "$PWD/config:/etc/tessera:ro" -v tessera-data:/var/lib/tessera \
  -e TESSERA_IDENTITY_KEY tessera build
docker run -d --name tessera --read-only --cap-drop ALL --security-opt no-new-privileges \
  -v "$PWD/config:/etc/tessera:ro" -v "$PWD/secrets:/run/secrets:ro" -v tessera-data:/var/lib/tessera \
  -p 8080:8080 -p 8081:8081 -p 127.0.0.1:8082:8082 tessera
```

Any other `tessera` verb runs the same way, with the same mounts: `check` to test the declaration against its source files, `verify /var/lib/tessera/bundle` to check a bundle.

## Health, logs and stopping

`/healthz` and `/readyz` answer on the viewer and session ports with no credential. The image's `HEALTHCHECK` runs `tessera health`, which asks the viewer port's `/readyz` and exits 0 on a 200, so `docker ps` shows the container as healthy once it is ready to serve. An orchestrator can probe the routes over HTTP directly instead.

Stop routing requests to an unhealthy container, and do not restart it. A server whose write-ahead log has failed reports not ready and keeps applying suppressions and deletions whose log write failed. A restart replays only what reached the log, so it can bring a hidden item back into view. Docker does not restart an unhealthy container itself.

Logs go to stderr as plain text. The first line on stdout is a JSON object naming the three bound addresses.

`docker stop` sends SIGTERM and the server exits at once. A write is fsynced to the log before it is acknowledged, so an acknowledged write survives the stop, and the next start replays the log.
