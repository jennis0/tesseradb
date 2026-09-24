# Put Tessera behind TLS

## Put nginx in front of the viewer address

Browsers should reach the viewer address over TLS, and from the same origin as the page that shows
the map. A reverse proxy does both. This nginx configuration serves your application and passes
`/v1/` to Tessera:

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

Here `127.0.0.1:8080` is your application, and `127.0.0.1:9151` is the viewer address (under the
Docker guide, `127.0.0.1:9161`). Only `/v1/` reaches Tessera, so the session address and the health
checks stay private.

A map response arrives in pieces: the counts for each tile first, then points as they are found,
then a final frame. `proxy_buffering off` has nginx pass each piece on as it arrives, so the browser
can start drawing before the response is complete. The viewer address refuses a request body over 2
MiB, and `client_max_body_size 2m` has nginx refuse at the same size. By default nginx gives up
after 60 seconds without hearing from Tessera, and Tessera ends any response that runs longer than
60 seconds (`serve.stream_deadline_ms`), so the default suits it.

The proxy must pass on every response header. nginx does unless you tell it otherwise, so add no
`proxy_hide_header` for any of these:

- `etag`
- `x-tessera-identity-key`
- `x-tessera-pin`
- `x-tessera-stale`
- `x-tessera-server-us`
- `x-tessera-admission-us`
- `x-tessera-region`
- `retry-after`, sent with a 429 when the server is busy

The browser client keeps what it has already been sent, filed under `x-tessera-identity-key`, and
reads `etag`, `x-tessera-pin` and `x-tessera-stale` to tell whether it is out of date. Without the
first, one person's cached map could be shown to the next person who signs in on the same page. To
check the headers come through:

```console
tessera$ TOKEN=$(curl -sS http://127.0.0.1:9152/session/authorise \
  -H "authorization: Bearer $(cat secrets/session.cred)" \
  -H 'content-type: application/json' \
  -d "{\"auth_data\": \"$(printf '{"terms": ["public"]}' | base64 -w0)\"}" | jq -r .token)
tessera$ curl -sS -o /dev/null -D - https://maps.example.org/v1/viewport \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"view": "ireland", "zoom": 0, "tiles": [0], "k": 0}'
HTTP/2 200 
server: nginx/1.18.0 (Ubuntu)
date: Thu, 24 Sep 2026 08:48:15 GMT
content-type: application/octet-stream
x-tessera-identity-key: 27d1f8b6d017ae6de0a6031d7895b89a
x-tessera-server-us: 12644
x-tessera-admission-us: 1
etag: "b6c42bd9060e5db3a6d7275e85d806c0"
x-tessera-pin: {"prefix":"v00000","segments_version":0}
x-tessera-stale: 0
```

### A page on another origin

If the page with the map comes from a different origin, such as `https://www.example.org` calling
the viewer at `https://maps.example.org`, the browser will only pass the response on if Tessera
names the page's origin. List it under `[serve]`:

```toml
cors_origins = ["https://www.example.org"]
```

The list applies to the viewer address only, and a page on those origins still cannot call the
session address. A wildcard is refused:

```text
tessera serve: refused to start: serve.cors_origins contains "*", which a CORS origin list cannot hold; list each origin, such as "https://app.example", or remove the key
```

Leave `dev_cors_origins` unset outside development. It opens the session address to the pages it
lists as well as the viewer, and the server logs a warning when it is set. `cors_loopback = true`
lets any page served from `localhost`, `127.0.0.1` or `[::1]` use the viewer address, which is what
a notebook needs.
