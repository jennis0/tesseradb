# Your first map from files

You will build a map of the 29,935 places GeoNames lists for Ireland, serve it, and filter it in a
browser by name, kind of place and population. You need Rust (installed with rustup), `git`,
`curl`, `unzip`, `openssl`, Python 3, and Node.js with npm. The commands below were run on Linux
with Rust 1.97, Python 3.12 and Node 22.

## Build the binary

Clone the repository and install `tessera` from it:

```bash
git clone https://github.com/jennis0/tesseradb ~/tesseradb
cd ~/tesseradb
cargo install --path crates/tessera-cli
```

```
...
    Finished `release` profile [optimized] target(s) in 1m 56s
...
```

Cargo puts the binary in `~/.cargo/bin`, which rustup adds to your `PATH`. The build took two
minutes on a 12-core machine.

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
describes the columns.

## Convert it to Parquet

`tessera build` reads Parquet files. Make a Python environment with pyarrow in it:

```bash
python3 -m venv .venv
.venv/bin/pip install pyarrow
```

Save the following as `convert.py`. It names the 19 columns as `readme.txt` lists them and writes
the six the map uses to `points.parquet`.

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

Save this as `corpus.toml` beside `points.parquet`:

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

[[attribute]]
name   = "population"
type   = "i64"
render = true
```

Paths in `[sources]` are relative to `corpus.toml`, and `[defaults]` makes `points` the file every
block below reads unless it names another.

The view needs an `extent`, the area of the map in degrees; there is no default. The build widens
it to the smallest map tile that contains it and moves any point outside that tile onto the tile's
edge. `point_visibility` names no field to read an access label from, so every place gets the
default label, `public`.

The vocabulary holds the nine GeoNames feature classes. Because it is `closed`, the build refuses
a place whose class is not in the list, and `public` lets every viewer see the whole list.

With `index = true`, the `name` column can be searched by word. `render = true` sends
`feature_class` and `population` with every point drawn, so the map can colour by them and filter
on them.

## Describe the deployment

`tessera.toml` says where the deployment keeps its files and which addresses the server listens
on. Save this beside `corpus.toml`:

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
token becomes a set of labels, and `builtin:passthrough` takes the labels the request names. Each
token the server issues lasts `token_max_lifetime` seconds unless it is revoked sooner.

The server listens on three addresses. The browser reads the map from the viewer address. The
session address hands out tokens to anyone holding the session credential, and the control address
takes writes from anyone holding the operator credential. `tessera check`, `tessera build` and
`tessera serve` find `tessera.toml` in the working directory or the nearest directory above it.

## Make the identity key and credentials

Clients see a `tessera_id` for each place instead of its `entity_id`, and the build derives it from
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
                       snapped outward to the square at z5 (15, 10) — x [0.46875, 0.5], y [0.3125, 0.34375]

views
  ireland                    labels from default_only, default 'public'

vocabularies
  feature_class              public, closed, 9 declared value(s)

attributes (in declaration order, which is the stored column order)
  name                       text, index, from column 'name'
  feature_class              u8 over vocabulary 'feature_class', hot, from column 'feature_class'
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
view 'ireland': web_mercator, quantising against x [0.46875, 0.5], y [0.3125, 0.34375]
        asked for lon [-11, -5], lat [51, 55.5] — snapped outward to the square at z5 (15, 10), lon [-11.25, 0], lat [48.922499263758255, 55.7765730186677]
        the data spans x [0.47030425, 0.5235287777777777], y [0.3141878520203559, 0.33315262108750776] — 62277 x 39773 of the 65536 x 65536 cells
        1 of 29935 point(s) (0.0%) CLAMP onto the frame's edge — 1 on x, 0 on y. A clamped point is stored at the boundary, not where it was written
        none of them outside web_mercator's ±85.0511287798066° domain, so nothing was clipped
...
attribute 'name': 29,935 of 29,935 entities have a value
attribute 'feature_class': 29,935 of 29,935 entities have a value
attribute 'population': 29,935 of 29,935 entities have a value
...
  view ireland: 29935 row(s)
```

The map tile runs from longitude −11.25 to 0, and the CLAMP line reports one place outside it. The
place is Saint Patrick's Bridge, a spit near Cork. GeoNames gives its longitude as 8.47036, where
Cork's is −8.47, so the build stored it on the eastern edge of the map. To move it, correct the
longitude in `IE.txt`, run `convert.py` again, delete the `bundle` directory and build again. The
build refuses to write over an existing bundle.

## Serve it

Run the server in a terminal of its own:

```bash
cd ~/ireland
tessera serve
```

```
2026-09-24T00:07:42.664932Z  INFO tessera_server::memory: the allocator's arena count is capped arenas=12
2026-09-24T00:07:42.715355Z  INFO tessera_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-24T00:07:42.715714Z  INFO tessera_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9141","session":"127.0.0.1:9142","control":"127.0.0.1:9143"}
```

The last line means all three addresses are open.

## Open the map

The map is drawn by the browser components in the repository's `clients/ts` directory. There is
no published package of them yet, so build them from the clone:

```bash
cd ~/tesseradb/clients/ts
npm ci
npm run build -w @tesseradb/components
```

```
...
dist/assets/decode.worker-DjqcC_7y.js    266.69 kB
dist/tessera-components.js             1,857.60 kB │ gzip: 444.08 kB

✓ built in 1.60s
...
```

`examples/plain-html` holds a page with the map on it and a small Node server for it. The server
holds the session credential and forwards the page's requests to Tessera, so the credential never
reaches the browser. It offers the users listed in `users.json`. Replace the example's users with
one who holds the label `public`:

```bash
cd examples/plain-html
echo '{"everyone": {"label": "Everyone", "terms": ["public"]}}' > users.json
```

In another terminal, point the server at your deployment and start it:

```bash
cd ~/tesseradb/clients/ts/examples/plain-html
export TESSERA_VIEWER_URL=http://127.0.0.1:9141
export TESSERA_SESSION_URL=http://127.0.0.1:9142
export TESSERA_SESSION_CRED=$(cat ~/ireland/session.secret)
node server.mjs
```

```
plain-html example on http://localhost:5180
```

Open <http://localhost:5180>. The page signs in as Everyone and draws every place in blue, and the
strip along the bottom reads 29,935 shown, 29,935 matched and 29,935 visible. The page has no base
map yet, so the outline of Ireland is the places themselves. The single point far to the east is
Saint Patrick's Bridge. Scroll to zoom in, and hold the pointer over a point to see its name.

## Filter the map

The panel on the left has a filter for each attribute you declared. The feature classes appear as
their letters; the [GeoNames feature codes page](http://www.geonames.org/export/codes.html) says
what each one means. P is a city, town or village.

Tick P. The matched count falls to 12,159, the places whose class is P. Then type 10000 in the
first Population box and press Enter: 72 places match. The largest is Dublin, with 1,024,027.

Choose Clear all, type `kilkenny` in the Name box and press Enter. Twelve places match. The search
matches whole words, so Kilkennybeg is not among them.

To colour the points by class, choose `feature_class` under Colour by.

Press Ctrl-C in each terminal to stop the page's server and Tessera.
