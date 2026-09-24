# Put Tessera behind TLS

Browsers should reach Tessera's viewer address over TLS, and preferably from the same origin as the
page that shows the map. Tessera doesn't terminate TLS itself, so this guide puts nginx in front of
it. It assumes an install from [the systemd guide](systemd.md), with the viewer address on
`127.0.0.1:9151`. Under [Docker](docker.md), point nginx at `127.0.0.1:9161` instead.

Only the viewer address goes behind the proxy. The session and control addresses stay private, as
[Addresses and credentials](operating.md#addresses-and-credentials) explains.

## Configure nginx

This configuration serves your application at `maps.example.org` and passes `/v1/` to Tessera:

```nginx
server {
    listen 443 ssl http2;
    server_name maps.example.org;

    ssl_certificate     /etc/ssl/maps.example.org/fullchain.pem;
    ssl_certificate_key /etc/ssl/maps.example.org/privkey.pem;

    location /v1/ {
        proxy_pass http://127.0.0.1:9151;
        proxy_http_version 1.1;
        proxy_buffering off;
        client_max_body_size 2m;
    }

    location / {
        proxy_pass http://127.0.0.1:8080;
    }
}
```

`127.0.0.1:8080` stands for your application. The health checks are not under `/v1/`, so they stay
private too.

A map response is streamed. The counts for each tile come first and the points follow as they are
found, with a final frame to mark the end. With `proxy_buffering off`, nginx passes each piece on as
it arrives, so the browser can start drawing before the response is complete.

nginx refuses a request body over 1 MiB unless told otherwise. A viewport request that names many
tiles can be larger than that, and the viewer address itself accepts up to 2 MiB, so
`client_max_body_size 2m` raises nginx's limit to match.

Both nginx and Tessera stop a response after 60 seconds, so leave nginx's timeout alone.

## Forward every header

nginx passes on every response header unless you tell it not to. Add no `proxy_hide_header` for
these:

- `etag`
- `x-tessera-identity-key`
- `x-tessera-pin`
- `x-tessera-stale`
- `x-tessera-server-us`
- `x-tessera-admission-us`
- `x-tessera-region`
- `retry-after`, sent with a 429 when the server is busy

The browser client keeps the answers it has already been sent, filed under
`x-tessera-identity-key`. Despite its name, that header is not the identity key. It is a value
derived from the viewer's grant and the view, and it changes when either does. Without it, one
person's cached map could be shown to the next person who signs in on the same page. The client
reads `etag`, `x-tessera-pin` and `x-tessera-stale` to tell whether what it holds is out of date.

To check the headers come through, get a token from the session address and ask for a viewport
through the proxy:

```console
tessera$ TOKEN=$(curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.secret)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}" | jq -r .token)
tessera$ curl -sS -o /dev/null -D - https://maps.example.org/v1/viewport \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"view": "ireland", "zoom": 0, "tiles": [0], "k": 0}'
HTTP/2 200 
server: nginx/1.18.0 (Ubuntu)
date: Thu, 24 Sep 2026 09:26:04 GMT
content-type: application/octet-stream
x-tessera-identity-key: 3a6acc325d736e5fc758f1ae29a489db
x-tessera-server-us: 363
x-tessera-admission-us: 1
etag: "e0bf26387bb9b086ad036a0f8206a42f"
x-tessera-pin: {"prefix":"v00000","segments_version":0}
x-tessera-stale: 0
```

## Let a page on another origin in

If the page with the map comes from a different origin, such as `https://www.example.org` calling
the viewer at `https://maps.example.org`, the browser hands it the response only if Tessera names
the page's origin. List it under `[serve]` in `tessera.toml`:

```toml
cors_origins = ["https://www.example.org"]
```

The list applies to the viewer address only. A page on those origins still cannot call the session
address, which has no cross-origin support. A wildcard is refused:

```text
tessera serve: refused to start: serve.cors_origins contains "*", which a CORS origin list cannot hold; list each origin, such as "https://app.example", or remove the key
```

Leave `dev_cors_origins` unset outside development. It opens the session address to the pages it
lists as well as the viewer address, and the server logs a warning when it is set.
`cors_loopback = true` lets any page served from `localhost`, `127.0.0.1` or `[::1]` use the viewer
address, which is what a notebook needs.
