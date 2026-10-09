# Your first map from files

This tutorial is for someone who has never used Mosaica. We'll take the 29,935 places GeoNames lists
for Ireland and put them on a map in your browser, where you can search them by name and filter them
by kind of place and by population.

This tutorial should take about 30 minutes to complete. You'll need Rust (installed with rustup),
`git`, `curl`, `unzip`, `openssl`, Python 3, Node.js with npm, and a web browser.

## What we'll do

1. Build `mosaica` from its source code.
2. Download the GeoNames file for Ireland and convert it to Parquet.
3. Write a declaration that describes the data, and validate it with `mosaica check`.
4. Build the data into a deployment with `mosaica build`.
5. Start the server with `mosaica serve`.
6. Open the map in a browser and filter it.

## Build Mosaica

Clone the repository, then build and install the `mosaica` binary with cargo.

```bash
git clone https://github.com/jennis0/mosaica ~/mosaica
cd ~/mosaica
cargo install --path crates/mosaica-cli
```

```
...
    Finished `release` profile [optimized] target(s) in 1m 56s
...
```

Cargo puts the program in `~/.cargo/bin`, which rustup has already added to your `PATH`, so you can
check straight away that it runs.

```bash
mosaica --version
```

```
mosaica 310920847827ba735cd8e953c898784853e84291
```

The long number is the commit your copy was built from, so yours will be different.

## Get the data

GeoNames is a free gazetteer of the world's place names, published as one file per country. Make a
directory for the project and download the file for Ireland.

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

`IE.txt` has one place per line, with 19 columns separated by tabs and no header row. `readme.txt`
explains what each column holds. Have a look at the first line.

```bash
head -n 1 IE.txt
```

```
2635367	Tullyrossmearan	Tullyrossmearan	Tullyrosmearn,Tullyrossmearan	54.33333	-7.98333	P	PPL	IE	GB	C	14			0		53	Europe/Dublin	2015-05-15
```

That's Tullyrossmearan, in County Leitrim. The line starts with its GeoNames id, followed by its
name, the same name in plain ASCII, and a list of other spellings. Then come its latitude and
longitude, and a `P` that marks it as a populated place. Further along, its population is given as
`0`.

A population of 0 means GeoNames doesn't have a figure, which is true of most places here. Only
1,119 of the 29,935 have a population at all. That will matter when we come to filter by it.

For this demo we'll keep five of the 19 columns: the name, the latitude and longitude, the class of
place and the population. We'll leave the GeoNames id behind. Mosaica doesn't need one to build a
map, and when we check the declaration we'll see what an id column would have changed.

## Convert it to Parquet

Mosaica's build reads Parquet, an efficient file format for tables. A Parquet file records the name and type of
every column inside the file itself, so a program can see what columns it has without reading any
rows. `mosaica check` makes use of that later on.

We'll do the conversion with pyarrow, a Python library for Parquet. Make a Python environment and
install pyarrow in it.

```bash
python3 -m venv .venv
source .venv/bin/activate
pip install pyarrow
```

Save this script as `convert.py`.

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
        column_types={"population": pa.int64()},
    ),
)

points = pa.table({
    "lon": table["longitude"],
    "lat": table["latitude"],
    "name": table["name"],
    "feature_class": table["feature_class"],
    "population": table["population"],
})
pq.write_table(points, "points.parquet")
print(f"wrote {points.num_rows} places to points.parquet")
```

Because the file has no header, the script supplies the 19 column names itself, in the order
`readme.txt` lists them. GeoNames doesn't quote its fields, so the script tells pyarrow not to look
for quotes. It reads the population as a whole number, then writes five columns to
`points.parquet` under the names we'll tell Mosaica to expect. Each row will be one place on the
map.

- `lon` and `lat` are the place's position in degrees.
- `name`, `feature_class` and `population` are what we want to see, search and filter on.

Run it.

```bash
python convert.py
```

```
wrote 29935 places to points.parquet
```

Every line of `IE.txt` is now a row in `points.parquet`.

## Describe the data

Next we tell Mosaica what the data means, in a file called a declaration. It's written in TOML. A
declaration names the files to read and says how to place each row on a map. It lists the columns to
keep and the kind of value each holds, and it says who may see each row. Save this as `corpus.toml` beside
`points.parquet`.

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

**The sources.** `[sources]` gives each file a short name, with its path relative to `corpus.toml`.
`[defaults]` makes `points` the source for every block below it, so we don't have to repeat it.
You only write these when building from files on disc. The Python client writes them for you from
the data you insert.

**The view.** A [view](../system/data-model.md#views-and-view-groups) is one way of laying the
places out on a flat map. It has a projection, which turns longitude and latitude into a position on
the map, and an extent, which is the part of the map it covers. A corpus can have several views. A
collection of documents might have a geographic map and a layout drawn from an embedding, for
instance, and a viewer can switch between them. We only need one, which we'll call `ireland`.

`web_mercator` is the projection almost every web map uses. It keeps shapes true, but stretches
areas more and more the further they are from the equator.

The `extent` gives a range of longitude and a range of latitude, in degrees. Ours is a rectangle
around Ireland. A declaration has to give an extent, but it can be `"auto"`, which fits it to the
data. We give a rectangle so that a place with a bad coordinate can't stretch the map.

`point_visibility` decides who may see each place. Mosaica gives each place an access label, and
each viewer holds a set of labels. A viewer sees only the places whose label they hold. Labels
usually come from a column in your data, but we don't have one, so every place gets the default
label, `public`. Every viewer holds `public`, so everyone will see every place. The tutorial on
access control gives places different labels and shows two people two different maps.

**The vocabulary.** A [vocabulary](../system/data-model.md#vocabularies) is the fixed list of values
a category attribute can take (more on attributes below). Here it's the feature class, a single
letter from a list GeoNames defines. The [GeoNames feature codes
page](http://www.geonames.org/export/codes.html) has more detail. The Ireland data has:

| Class | What GeoNames files under it | Places |
|---|---|---|
| A | counties and other administrative areas | 112 |
| H | lakes, rivers, bays and other water | 3,241 |
| L | parks, areas and localities | 3,507 |
| P | cities, towns and villages | 12,159 |
| R | roads and railways | 106 |
| S | buildings, farms and other spots | 6,722 |
| T | hills, mountains, islands and other landforms | 3,970 |
| U | undersea features | 0 |
| V | forests and heaths | 118 |

`width = "u8"` stores each place's class as a one-byte code, which leaves room for 255 values. For
longer lists there are `u16` and `u32`.

`value_set = "closed"` says the list is complete. The build will refuse any place whose class isn't
on it. An open vocabulary is derived from the data instead, and gains a new entry whenever a new
value turns up.

`visibility = "public"` lets every viewer see the whole list, including `U`, which no Irish place
carries. The alternative, `derived`, would show each viewer only the values found on places they can
see. This `public` is a setting on the vocabulary itself, and has nothing to do with the access
label on each place.

**The attributes.** An attribute is a property Mosaica keeps for every place, which you can use to
draw, filter or highlight the data. We declare three. The name is `text`, the feature class is a
`category` drawn from the vocabulary above, and the population is an `i64`, which is a whole number.

Unlike a traditional database, Mosaica is built to stream millions of points to the user's browser
for an interactive map. Most attributes aren't needed to draw the map, so when you build a database
you tell Mosaica what each attribute is for. There are two settings.

- `render = true` stores the value beside each place's position on the map, so it travels with
  every point that's drawn. Any value the map uses when drawing, such as the colour of each point,
  needs it. We set it on `feature_class` and `population`.
- `index = true` builds a search index for filtering the data. On `name`, it lets you filter places
  with a text search.

An attribute with neither setting can still be read for each place, but it can't be drawn or used
for filtering. It takes less memory and less disk space than one with either.

## Describe the deployment

The declaration describes the data. A second file, `mosaica.toml`, configures the database itself.
It says where the bundle goes and which declaration to build, and holds the rest of the system's
settings. `mosaica check`, `mosaica build` and `mosaica serve` all look for it in the directory you
run them from, and then in the directories above. Save this beside `corpus.toml`.

```toml
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

[build]
schema = "corpus.toml"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer                   = "127.0.0.1:9141"
session                  = "127.0.0.1:9142"
control                  = "127.0.0.1:9143"
operator_credential_file = "operator.secret"

[catalogue]
dir = "catalogue"
```

`[bundle]` names three places on disc. `path` is the directory the build writes and the server
opens. `cache` is where the server keeps work it can reuse, such as each viewer's set of places.
`wal` is the server's write-ahead log, where it records every change to the data before applying it,
so that a change survives a crash.

`[build]` names the declaration to build.

`token_max_lifetime` is how long a browser's permission to read the map lasts, in seconds. An hour
is plenty here.

`[serve]` gives the server's three addresses and the name of a file holding the operator's secret.
We'll create the file, and explain the addresses, when we start the server.

`[catalogue]` names the directory where Mosaica keeps its users, the passwords and keys they sign
in with, and what each may see and do. The server creates it the first time it starts.

## Check the declaration

Before building anything, ask Mosaica what it makes of the two files. `mosaica check` reads the
declaration, opens each Parquet file it names, and compares the columns the declaration asks for
with the columns each file actually has. It only reads names and types, never rows, so it's quick.

```bash
mosaica check
```

```
...
  identity     source 'points': names items by nothing: each row is an item of its own
  identity     view 'ireland': names items by nothing: each row is an item of its own
projected views, from the declaration alone:
  ireland              web_mercator, asked for lon [-11, -5], lat [51, 55.5]
                       snapped outward to the square at z5 (15, 10) — lon [-11.25, 0], lat [48.922499263758255, 55.7765730186677]

views
  ireland                    labels from default_only, default 'public'

vocabularies
  feature_class              public, closed, 9 declared value(s)

attributes
  name                       text, index, from column 'name'
  feature_class              u8 over vocabulary 'feature_class', render, from column 'feature_class'
  population                 i64, render, from column 'population'

check OK: 2 source(s), 1 view(s), 0 view group(s) over 0 declared view(s), 1 vocabulary(ies), 3 attribute(s), 0 layer(s), 0 warning(s)
```

`check OK` on the last line is what we want to see. Above it, the check describes what the build is
going to do.

The last line counts two sources, though we only have one file. The check reads the file's schema
twice, once for the attributes and once for the view's positions.

The `identity` lines, one for each reading, say how the build will tell one place from another. A
database usually does that with a key. Mosaica's equivalent is an attribute declared
`unique = true`, such as the GeoNames id. A row carrying an id that a place already holds then
names that place and changes it. We declared no unique attribute, so nothing in our file names a
place, and each row becomes a place of its own. That's all a map built once from one file needs.
If you later wanted to correct or delete a place by its GeoNames id, you'd keep the `geonameid`
column in `convert.py` and declare it as an `i64` attribute with `unique = true`. Mosaica would
then keep an index from each id to its place.

The projected views section holds a surprise. We asked for a rectangle, but the check says the
view has been snapped outward to a square. Mosaica stores positions on a grid that lines up with the
standard map tiles, so that any tile the map asks for, at any zoom, falls exactly on the grid. To
make that work it widens our extent to the smallest standard tile that contains it. At zoom level 5
the world is 32 tiles across, and ours is the tile in column 15, row 10, counting from zero at the
top left. In degrees, that tile runs from longitude −11.25 to 0 and from latitude 48.9 to 55.8.

The rest repeats what we declared. `labels from default_only` means no column supplies access
labels, so every place gets the default, `public`.

While nothing depends on it yet, try making a mistake. Open `corpus.toml`, change the last
attribute's name to `populaton`, and run the check again.

```bash
mosaica check
```

```
...
  FAILED       attribute 'populaton': declared type 'i64', read from a column named 'populaton', which source 'points' does not carry. Its columns are: lon, lat, name, feature_class, population
...
check FAILED: 1 finding(s) across 2 source(s). Nothing was read but Parquet schemas, so a clean check is not a clean build: it cannot see a value against a closed vocabulary, which rows the identity rule refuses, or where the data sits inside a view's extent
```

The check names the attribute, says which column it went looking for, and lists the columns the file
does have. Its last line is honest about its limits. It has only seen names and types, so a class
missing from our closed vocabulary would get past it, and only the build would catch that. Change
the name back to `population` and run `mosaica check` once more to get `check OK` back.

## Build the bundle

Inside the server, every place is identified by a number of Mosaica's own. That number never
leaves the server, because a viewer who collected a few of them could estimate how many places
they aren't allowed to see. Browsers get a `tessera_id` for each place instead. It's the internal
number scrambled with a secret key, which the build makes up at random and keeps inside the bundle.
There is nothing for you to set up. The [security
chapter](../system/security.md#a-client-never-sees-an-entity-id) explains what the scrambling
hides and what it doesn't. Each build makes a new key, so building again gives every place a new
`tessera_id`, and any `tessera_id` a viewer had saved will no longer point at the same place.

Now build the bundle. The build reads every row of `points.parquet` and works out where each place
sits on the map. It stores neighbouring places next to each other on disc, so that the places in any
map tile, at any zoom, are efficiently retrieved. Along the way it checks every feature class
against the vocabulary, builds the search index over the names and records which places carry which
access label. Everything goes into the `bundle` directory. From here on the server reads only the
bundle, and never looks at your Parquet file again.

```bash
mosaica build
```

```
view 'ireland': web_mercator, quantising against x [0.46875, 0.5], y [0.3125, 0.34375]
        asked for lon [-11, -5], lat [51, 55.5] — snapped outward to the square at z5 (15, 10), lon [-11.25, 0], lat [48.922499263758255, 55.7765730186677]
        the data spans x [0.47030425, 0.5235287777777777], y [0.3141878520203559, 0.33315262108750776] — 62277 x 39773 of the 65536 x 65536 cells
        1 of 29935 point(s) (0.0%) CLAMP onto the frame's edge — 1 on x, 0 on y. A clamped point is stored at the boundary, not where it was written
        none of them outside web_mercator's ±85.0511287798066° domain, so nothing was clipped
...
1 view(s): 0 point row(s) carried access terms of their own; 29935 took the declared default
...
attribute 'name': 29,935 of 29,935 entities have a value
attribute 'feature_class': 29,935 of 29,935 entities have a value
attribute 'population': 29,935 of 29,935 entities have a value
...
built /home/you/ireland/bundle (v00000): 29935 items, 1 terms, 29935 pairs, 2197486 bytes on disk, 0 artifact(s) minted, 0 unclustered member row(s), 0 row(s) refused
  view ireland: 29935 row(s)
```

It takes a few seconds. Most of the report confirms what we expected. All 29,935 places took the
default label, and every one of them has a value for each attribute. Every feature class was one of
our nine letters, too. If one hadn't been, the build would have stopped and named it.

The second line gives the snapped tile in degrees, as the check did. The fourth line says one place
fell outside that tile. The build has clamped it, which means it moved the place onto the nearest
edge of the tile instead of dropping it.

That place is Saint Patrick's Bridge. GeoNames lists three places of that name in Ireland. One is a
spit on the Wexford coast, and another is the bridge over the Lee in Cork, filed as a historic site
at longitude −8.47036. The third has the spit's class and the Cork bridge's position, except that
its longitude is 8.47036. The minus sign has gone missing, which puts it east of Greenwich, in
Germany. The build has pinned it to the eastern edge of our map.

We'll leave it there, since it makes a handy landmark. To correct it, you'd fix the longitude in
`IE.txt`, run `convert.py` again, delete the `bundle` directory and build again. The build won't
write over an existing bundle.

The line beginning `built` names the new bundle's directory, which on your machine is the full
path of your own `ireland` directory; we've written it as `/home/you/ireland` here. It counts
`29935 items`, one for each row, as the check's `identity` lines promised, and ends with
`0 row(s) refused`. A build refuses a row that names a place an earlier row already named, such as
a second row carrying the same GeoNames id. It keeps the first row, leaves the later one out, prints
what it left out and writes the list to `bundle/reports/refused.json`. With no unique attribute,
nothing in our file can name a place twice.

## Serve it

The server needs the secret file `mosaica.toml` names. Fill it with a random value that only you
can read.

```bash
openssl rand -hex 16 > operator.secret
chmod 600 operator.secret
```

The server listens on three addresses, each with its own job.

- The viewer address, port 9141, serves the data. Each caller gets only the places it's allowed to
  see.
- The session address, port 9142, gives a caller permission to read the map. It only answers
  callers holding a key that may act for other users, or the operator secret.
- The control address, port 9143, takes changes to the data, such as new places and deletions,
  and changes to the users. It only answers callers holding a key or a secret that allows the
  change. We'll use it once, to create the users the map's page needs.

Keeping them apart means you can open each address only to the callers that need it.

`mosaica serve` opens the bundle and starts answering on all three. It keeps running until you stop
it, so give it a terminal of its own.

```bash
cd ~/ireland
mosaica serve
```

```
2026-09-24T00:49:40.984688Z  INFO mosaica_server::memory: the allocator's arena count is capped arenas=12
2026-09-24T00:49:41.027452Z  INFO mosaica_engine::engine: the engine adopted the prefix's derived artifact structures named=0 containment_adopted=0 prefix=v00000
2026-09-24T00:49:41.027864Z  INFO mosaica_server: bulk reads may hold this much memory at once, within the process's memory cap bulk_admission=2 max_page_bytes=67108864 bulk_read_memory_bytes=939524096
{"event":"listening","viewer":"127.0.0.1:9141","session":"127.0.0.1:9142","control":"127.0.0.1:9143"}
```

The three `INFO` lines are the server describing its own set-up, and you can ignore them. The last
line is the one to look for. It means all three addresses are open.

## Open the map

The map itself is drawn by Mosaica's browser components, which live in the repository's `clients/ts`
directory. They aren't published as a package yet, so build them from the clone. Use a second
terminal for this.

```bash
cd ~/mosaica/clients/ts
npm ci
npm run build -w @mosaica/components
```

```
...
dist/assets/decode.worker-DjqcC_7y.js    266.69 kB
dist/mosaica-components.js             1,857.60 kB │ gzip: 444.08 kB

✓ built in 3.75s
...
```

`examples/plain-html` holds a web page with the map on it, and a small Node server that signs you
in. Mosaica needs to know who you are signed in as. It keeps its users, which it calls principals,
in the catalogue, and the page's server asks the session address for a token on your behalf.

So we need two principals. `everyone` is the person reading the map. It holds the `read`
permission, which lets it read, and no labels of its own, since every place is labelled `public`
and every principal holds that. `page` is the page's server. It holds `authorise-as`, which lets it
ask for a token for another principal. Changes to the catalogue go to the control address, with the
operator secret.

```bash
cd ~/ireland
export MOSAICA_CREDENTIAL=$(cat operator.secret)
mosaica principal create everyone --kind person --control http://127.0.0.1:9143
mosaica grant --principal everyone --permission read --control http://127.0.0.1:9143
mosaica principal create page --kind service --control http://127.0.0.1:9143
mosaica grant --principal page --permission authorise-as --control http://127.0.0.1:9143
```

```
{"sessions_ended":0}
{"sessions_ended":0}
{"sessions_ended":0}
{"sessions_ended":0}
```

Each change answers how many sessions it ended. Nobody is signed in yet, so the answer is 0.

The page's server proves it is `page` with an API key. Mosaica shows a key once, when it makes it,
so save it straight to a file only you can read.

```bash
umask 077
mosaica key create page --control http://127.0.0.1:9143 > page.key
cat page.key
```

```
{"key":"tsk_92ce602a0958_c37c39511c94d1f5dbe414cdd9dde1232c602409e2db22d01401afc0b299d46a","prefix":"92ce602a0958"}
```

The page's server signs you in as one of the people listed in `users.json`, each with the
principal it reads as. The list that comes with the example belongs to a different dataset, so
replace it with a single person, Everyone.

```bash
cd ~/mosaica/clients/ts/examples/plain-html
echo '{"everyone": {"label": "Everyone", "principal": "everyone"}}' > users.json
```

Tell the page's server where Mosaica is listening and give it the key, then start it.

```bash
export MOSAICA_VIEWER_URL=http://127.0.0.1:9141
export MOSAICA_SESSION_URL=http://127.0.0.1:9142
export MOSAICA_API_KEY=$(python3 -c 'import json; print(json.load(open("/dev/stdin"))["key"])' < ~/ireland/page.key)
node server.mjs
```

```
plain-html example on http://localhost:5180
```

Open <http://localhost:5180>. The page signs you in as Everyone and draws every place in blue.
There's no base map yet, so the outline of Ireland is drawn by the places themselves. The gap in the
north-east is Northern Ireland, which GeoNames files under the United Kingdom, and the lone point
far out to the east is Saint Patrick's Bridge on the edge of our tile. Scroll to zoom in, and hover
over a point to see its name.

The strip along the bottom reads 29,935 shown, 29,935 matched and 29,935 visible. Visible is how
many places you're allowed to see in the part of the map on screen. Matched is how many of those
pass your filters, which with no filters set is all of them. Shown is how many points the page has
actually drawn. It can be lower than matched, because where an area holds more matches than the page
draws at once, the server sends a sample.

If you leave the page open for more than an hour, the strip will say Session expired. Reload the
page to sign in again.

## Filter the map

The panel on the left has a filter for each attribute we declared. There's a search box for the name
and a range for the population. Each feature class has its own tick box, and that's where we'll
start.

Tick P. The matched count drops to 12,159, the same number of cities, towns and villages as in the
table.

Now type 10000 in the first Population box and press Enter. The count falls to 72, the places with
ten thousand people or more. The largest of them is Dublin, with 1,024,027. A village with no
population figure counts as 0, so it falls outside the range.

The visible count drops while a filter is on. With P ticked it fell to 29,687 in our window, because
the page only counts visible places where it has drawn something, and some stretches of the map have
no town at all. Matched is the number to watch.

Choose Clear all, type `kilkenny` in the Name box and press Enter. Twelve places match. There's the
county, the city and a village of the same name over in County Mayo, and then seven hotels, the
airport and the Smithwick's brewery. The search matches whole words, so the locality of Kilkennybeg
isn't among them.

Choose Clear all again, then pick `feature_class` under Colour by. Each class gets its own colour,
using the value that `render = true` sends with every point.

When you've finished, press Ctrl-C in each terminal to stop the page's server and Mosaica.

## What you built

`~/ireland` now holds a working deployment. The data is in `points.parquet` and the declaration in
`corpus.toml`. `mosaica.toml` holds the deployment's settings, `operator.secret` the operator's
secret, and `page.key` the page's key. The `catalogue` directory holds the two principals, and the
`bundle` directory is what the build made from the data. To bring the map back, start
`mosaica serve` in `~/ireland`, then run the three `export` lines and `node server.mjs` in the
example's directory. The principals and the key are still in the catalogue.

## What you learned

- A declaration tells Mosaica what your data means. `mosaica check` compares it with the column
  names and types in your files, and `mosaica build` reads every row.
- The bundle is what the server serves. The build sorts the places into map order and writes the
  indexes the server needs, and after that the server never opens your source files.
- Every place carries an access label, and a viewer sees only the places whose label they hold.
  Ours all carry `public`, which everyone holds.
- The server listens on three addresses. Browsers read the map from the viewer address, your own
  server gets them permission from the session address, and changes to the data and to the
  catalogue of principals go to the control address.
- Everything a viewer is sent, every count and every point, is computed from the places their
  labels allow.

## Next

The next tutorial is about access control. It gives places different labels, signs in two people who
hold different ones, and compares their maps. It also explains the permission a browser gets, which
Mosaica calls a token. That tutorial isn't written yet. In the meantime, the
[overview](../system/overview.md) describes the whole system, and [the data
model](../system/data-model.md) says more about views, attributes and vocabularies. [Access
control](../system/access-control.md) covers labels and tokens.
