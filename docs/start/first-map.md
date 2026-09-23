# Your first map from files

You will build a map of the 29,935 places GeoNames lists for Ireland, serve it, and filter it in a
browser by name, kind of place and population. You need the `tessera` binary on your `PATH`,
Python 3, `git`, and Node.js with npm. The commands below were run on Linux with Python 3.12 and
Node 22.

## Download the extract

GeoNames publishes one file per country. Make a directory for the project and download Ireland's:

```bash
mkdir ~/ireland && cd ~/ireland
curl -sO https://download.geonames.org/export/dump/IE.zip
unzip IE.zip
```

```
Archive:  IE.zip
  inflating: readme.txt              
  inflating: IE.txt                  
```

`IE.txt` holds one place per line, in 19 tab-separated columns with no header row. `readme.txt`
names the columns.

## Convert it to Parquet

`tessera build` reads Parquet files. Make a Python environment with pyarrow in it:

```bash
python3 -m venv .venv
.venv/bin/pip install pyarrow
```

Save the following as `convert.py`. It reads `IE.txt` with the column names from `readme.txt` and
writes the six columns the map uses to `points.parquet`.

```{.python notest}
import pyarrow as pa
import pyarrow.csv as csv
import pyarrow.parquet as pq

columns = [
    "geonameid", "name", "asciiname", "alternatenames", "latitude", "longitude",
    "feature_class", "feature_code", "country_code", "cc2", "admin1_code",
    "admin2_code", "admin3_code", "admin4_code", "population", "elevation", "dem",
    "timezone", "modification_date",
]

table = csv.read_csv(
    "IE.txt",
    read_options=csv.ReadOptions(column_names=columns),
    parse_options=csv.ParseOptions(delimiter="\t", quote_char=False),
    convert_options=csv.ConvertOptions(
        column_types={"geonameid": pa.uint64(), "population": pa.int64()},
    ),
)

points = pa.table({
    "entity_id": table["geonameid"],
    "lon": table["longitude"],
    "lat": table["latitude"],
    "name": table["name"],
    "feature_class": table["feature_class"],
    "population": table["population"],
})
pq.write_table(points, "points.parquet")
print(f"wrote {points.num_rows} places to points.parquet")
```

The build takes each row's identity from a column called `entity_id`, so the script gives the
GeoNames id that name. It reads positions from `lon` and `lat`, in degrees.

```bash
.venv/bin/python convert.py
```

```
wrote 29935 places to points.parquet
```

## Declare the corpus

The corpus declaration says which files to read, how to place each row on a map, and which
columns to serve with it. Save this as `corpus.toml` beside `points.parquet`:

```toml
[sources]
points = "points.parquet"

[defaults]
source = "points"

[[view]]
name             = "ireland"
projection       = "web_mercator"
extent           = { lon = [-11.0, -5.0], lat = [51.0, 55.5] }
point_visibility = { default = "public" }

[[vocabulary]]
name       = "feature_class"
width      = "u8"
value_set  = "closed"
visibility = "public"
values     = ["A", "H", "L", "P", "R", "S", "T", "U", "V"]

[[attribute]]
name  = "name"
type  = "text"
index = true

[[attribute]]
name       = "feature_class"
type       = "category"
vocabulary = "feature_class"
render     = true
index      = true

[[attribute]]
name   = "population"
type   = "i64"
render = true
```

`[sources]` names each file, relative to `corpus.toml`. `[defaults]` makes `points` the file that
every block below reads unless it names another.

A `[[view]]` is one map of the data. `web_mercator` is the projection that web maps use; the build
projects each row's `lon` and `lat` through it. `extent` is the area the map covers, in degrees.
It has no default. The build widens it to the smallest map tile that contains it and moves any
point outside that tile onto the tile's edge. `point_visibility`
decides who may see each point. This file gives no field to read a label from, so every place
carries the label `public`.

A `[[vocabulary]]` lists the values a category column may hold: here, the nine GeoNames feature
classes. `u8` stores each value in one byte. `closed` makes the build refuse a value that is not
in the list, and `public` lets every viewer see the whole list.

Each `[[attribute]]` is a column served with the places. `name` is text, and `index = true` makes
it searchable by word. `feature_class` is a category over the vocabulary above; `render = true`
sends it with every point drawn, so the map can colour by it, and `index = true` lets you filter
on it. `population` is a 64-bit integer, sent with every point drawn.

## Describe the deployment

A deployment is one bundle and the server that serves it. `tessera.toml` says where its files live
and which addresses it listens on. Save this beside `corpus.toml`:

```toml
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

[build]
schema = "corpus.toml"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer                   = "127.0.0.1:9141"
session                  = "127.0.0.1:9142"
control                  = "127.0.0.1:9143"
session_credential_file  = "session.secret"
operator_credential_file = "operator.secret"
```

`[bundle]` names the directory the build writes and the server opens, a directory for the server's
cache, and the log the server writes changes to before applying them. `[build]` points at the
declaration; without it, the build looks for `schema.toml`. `[plugin]` decides how a request for a
token becomes a set of labels, and `builtin:passthrough` takes the labels the request names.
`token_max_lifetime` is how long a viewer's token lasts, in seconds.

The server listens on three addresses. The browser reads the map from the viewer address. The
session address hands out tokens to anyone holding the session credential, and the control address
takes writes from anyone holding the operator credential. `tessera check`, `tessera build` and
`tessera serve` find `tessera.toml` in the working directory or the nearest directory above it.

## Make the identity key and credentials

Clients never see `entity_id`. They see a `tessera_id` for each place, which the build derives from
the identity key. `tessera build` reads the key from a file named `.env` beside `tessera.toml`. The
two credentials go in the files `tessera.toml` names.

```bash
echo "TESSERA_IDENTITY_KEY=$(openssl rand -hex 16)" > .env
openssl rand -hex 16 > session.secret
openssl rand -hex 16 > operator.secret
chmod 600 .env session.secret operator.secret
```

Keep `.env`. A build with a different key gives every place a different `tessera_id`.

## Check the declaration

`tessera check` compares the declaration with the schema of each Parquet file, without reading the
rows, and prints what the build will make of it:

```bash
tessera check
```

```
...
projected views, from the declaration alone:
  ireland              web_mercator, asked for lon [-11, -5], lat [51, 55.5]
...

views
  ireland                    labels from default_only, default 'public'

vocabularies
  feature_class              public, closed, 9 declared value(s)

attributes (in declaration order, which is the stored column order)
  name                       text, index, from column 'name'
  feature_class              u8 over vocabulary 'feature_class', hot+index, from column 'feature_class'
  population                 i64, hot, from column 'population'

check OK: 2 source(s), 1 view(s), 0 view group(s) over 0 declared view(s), 1 vocabulary(ies), 3 attribute(s), 0 layer(s), 0 warning(s)
```

`hot` in the list of attributes is the check's word for `render = true`. A declaration with a
required key missing stops the check at that key. Without `width` in the vocabulary, for instance,
it prints this and exits with status 1:

```
check FAILED: schema: vocabulary 'feature_class': `width` is required: `u8`, `u16` or `u32`
```

Add the key it names and run the check again.

## Build the bundle

```bash
tessera build
```

```
View 'ireland': web_mercator, quantising against x [0.46875, 0.5], y [0.3125, 0.34375]
...
tessera build: commit d15e9d2df9ab498ceb0c71f61fbd247e475dbefe
1 view(s): 0 point row(s) carried access terms of their own; 29935 took the declared default
...
attribute 'name': 29,935 of 29,935 entities have a value
attribute 'feature_class': 29,935 of 29,935 entities have a value
attribute 'population': 29,935 of 29,935 entities have a value
...
  view ireland: 29935 row(s)
```

The bundle is 2,255,482 bytes on disk. Its map tile runs from longitude −11.25 to 0, and one of the
lines cut above reports a place outside it:
`1 of 29935 point(s) (0.0%) CLAMP onto the frame's edge — 1 on x, 0 on y`. The place is Saint
Patrick's Bridge, a spit near Cork. GeoNames gives its longitude as 8.47036, where Cork's is
−8.47, so the build stored it on the eastern edge of the map. To move it, correct the longitude in
`IE.txt`, run `convert.py` again, delete the `bundle` directory and build again. The build refuses
to write over an existing bundle.

## Serve it

Run the server in a terminal of its own, from `~/ireland`:

```bash
tessera serve
```

```
2026-09-23T23:44:56.858183Z  INFO tessera_server::memory: the allocator's arena count is capped arenas=12
2026-09-23T23:44:56.894068Z  INFO tessera_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-23T23:44:56.894374Z  INFO tessera_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9141","session":"127.0.0.1:9142","control":"127.0.0.1:9143"}
```

The last line means all three addresses are open. The server runs until you stop it.

## Open the map

The map is drawn by the browser components in the repository's `clients/ts` directory. Not built
yet: a published package of them. Build them from a copy of the repository instead:

```bash
git clone https://github.com/jennis0/tesseradb ~/tesseradb
cd ~/tesseradb/clients/ts
npm ci
npm run build -w @tesseradb/components
```

```
...
dist/tessera-components.js             1,857.60 kB │ gzip: 444.08 kB

✓ built in 894ms
...
```

`examples/plain-html` holds a page with the map on it and a small Node server for it. The server
keeps the session credential, asks Tessera for a token on behalf of the page's user, and passes
the page's requests to the viewer address, so the browser talks only to the page's own server.
It offers the users listed in `users.json`. Replace them with one user who holds the label
`public`:

```bash
cd examples/plain-html
echo '{"everyone": {"label": "Everyone", "terms": ["public"]}}' > users.json
```

Point the server at your deployment and start it in another terminal:

```bash
export TESSERA_VIEWER_URL=http://127.0.0.1:9141
export TESSERA_SESSION_URL=http://127.0.0.1:9142
export TESSERA_SESSION_CRED=$(cat ~/ireland/session.secret)
PORT=5190 node server.mjs
```

```
plain-html example on http://localhost:5190
```

Open <http://localhost:5190>. The page signs in as Everyone and draws every place in blue, and the
strip along the bottom reads 29,935 shown, 29,935 matched and 29,935 visible. The outline of
Ireland is the places themselves. Not built yet: a base map under the points on this page. The
single point far to the east is Saint Patrick's Bridge. Scroll to zoom in, and hold the pointer
over a point to see its name.

## Filter the map

The panel on the left has a filter for each attribute you declared. The feature classes appear as
their letters; the [GeoNames feature codes page](http://www.geonames.org/export/codes.html) says
what each one means. P is a city, town or village.

Tick P. The matched count falls to 12,159, the places whose class is P. Then type 10000 in the
first Population box and press Enter. The two filters combine, and 72 places match. The largest is
Dublin, with 1,024,027.

Choose Clear all, type `kilkenny` in the Name box and press Enter. Twelve places match. The search
matches whole words, so Kilkennybeg is not among them.

To colour the points by class, choose `feature_class` under Colour by.

Press Ctrl-C in each terminal to stop the page's server and Tessera.
