<!-- Generated from crates/tessera-build/src/config.rs. Edit the doc comments there, then run: TESSERA_WRITE_CORPUS_REFERENCE=1 cargo test -p tessera-build corpus_reference -->

# corpus.toml

`corpus.toml` declares a corpus: the files a build reads, the views that place each item on a map, the vocabularies and attributes each item carries, and the annotation layers drawn over the items. `tessera build` and `tessera check` read the file that `[build] schema` in `tessera.toml` names, `schema.toml` by default, or the one `--config` names. `tessera check --payloads` prints the same declaration as the request bodies that declare it on a running service.

Every table refuses a key it does not know. Where a key is refused beside another, or needs another, its description says so.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `sources` | table of strings | not set | The files the declaration reads, as `name = "path"`, each path relative to the directory the declaration is in. Every `source` key elsewhere names one of these. An empty name, an empty path and an absolute path are refused; `--file NAME=PATH` replaces a path from the command line. |
| `defaults` | table | not set | The source and entity id column a block takes when it names none. |
| `view` | array of tables | `[]` | Coordinate systems, each giving the items it holds a position on a map. |
| `view_group` | array of tables | `[]` | Sets of views that share every setting and differ by a key. |
| `vocabulary` | array of tables | `[]` | Named value sets, which `category` attributes draw on. |
| `attribute` | array of tables | `[]` | The columns each item carries. |
| `layer` | array of tables | `[]` | Annotation layers: named sets of artifacts, such as clusters or regions, drawn over views. |

## `[defaults]`

What a block takes when it names no source or entity id column of its own. `source` reaches a `[[view]]` and an entity-scoped `[[attribute]]`, and nothing else: a vocabulary, a layer, a view group and `point_visibility` with no source of their own read no file.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `source` | string | not set | A name in `[sources]`, read by a `[[view]]` and an entity-scoped `[[attribute]]` that name no `source`. A name `[sources]` does not have is refused. |
| `entity_id_field` | string | `"entity_id"` | The column holding the entity id in a view's points and an attribute's source, where the block does not name one itself. An empty name is refused. |
| `allocation_view` | string | not set | The view whose map positions order the entity ids a build assigns to items with the same access labels. A build of more than one view, counting each view of a group, is refused without it. A view of a group is named `<group>:<key>`, and a name that is not one of the build's views is refused. |

## `[[view]]`

One coordinate system: a position for each item it holds, the frame those positions are stored across, and who may see the view and each of its points.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The view's name, which a request names it by. ASCII letters, digits, `_` and `-`, unique among views and view groups. |
| `title` | string | not set | A display title. Not built yet: the title is accepted and not published. |
| `projection` | string | `"none"` | How a longitude and latitude become a position on the map: `web_mercator`, `equirectangular` (also written `plate_carree`), `gall_isographic`, or `none` for coordinates that are not places on the Earth. It decides which spellings `extent` and `fields` take. Any other name is refused. |
| `source` | string | the value of `[defaults].source` | A name in `[sources]`: the file holding the view's points, one row per item. A build refuses a view with no source; `tessera check` accepts one. |
| `fields` | table of strings | not set | Where the points file keeps each field, as `field = "column"`. The fields are `entity_id` with either `x` and `y` or `morton` and `residual`; a projected view's are `entity_id`, `lon` and `lat`. A field not named here is read from the column of its own name. A field the view does not have, both kinds of position, `residual` without `morton`, and `fields` where the view has no source are refused. |
| `extent` | string or table | required | The frame positions are stored across, as a 32-bit position on each axis. A point outside it is stored on its edge, and a build refuses a frame that more than half the points fall outside. The spellings are under `[view.extent]`. |
| `point_visibility` | table | required | Where each point's access label comes from: keys under `[view.point_visibility]`. |
| `visibility` | string or array of strings | `"public"` | The access label a viewer must hold to reach the view, or a list of labels of which they must hold one. `public` alone admits every viewer. An empty list, an empty label, `inherited`, `public` beside another label, and a label the plugin maps to no term are refused. |

## `[view.extent]`

`extent` is the word `"auto"` or a table. A view with no projection takes four spellings, in the units of its own coordinates:

```toml
extent = "auto"                           # the same as { auto = true, margin = 0.01 }
extent = { auto = true, margin = 0.25 }   # a square fitted to the data, plus margin each side
extent = { min = -25.0, max = 25.0 }      # one range for both axes
extent = { x = [-18, 19], y = [-22, 24] } # a range for each axis
```

A projected view takes two, in degrees, and widens the box to the smallest aligned square that contains it:

```toml
extent = "auto"                                     # the data's own longitude and latitude
extent = { lon = [-8.6, 1.8], lat = [49.9, 60.9] }  # a box
```

`auto` reads the view's points to fit the frame, so it is refused where the points source holds no rows. Any other word, an empty table, half a box, a number that is not finite, and a spelling of the other kind of view are refused.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `auto` | boolean | not set | `true` fits the frame to the points the build reads. `false` is refused, and so is `auto` beside `min`, `max`, `x` or `y`. A projected view writes `extent = "auto"` instead. |
| `margin` | number | `0.01` | Space added to each side of an `auto` frame, as a fraction of the data's span. Finite and at least 0. Refused without `auto = true`. |
| `min` | number | not set | The low end of both axes, beside `max`. |
| `max` | number | not set | The high end of both axes, beside `min`. It must be above `min`. |
| `x` | array of 2 numbers | not set | The x axis as `[low, high]`, beside `y`, with `high` above `low`. |
| `y` | array of 2 numbers | not set | The y axis as `[low, high]`, beside `x`, with `high` above `low`. |
| `lon` | array of 2 numbers | not set | A projected view's longitudes as `[west, east]`, in degrees within ±180, beside `lat`. A box crossing the antimeridian, with `west` above `east`, is refused. |
| `lat` | array of 2 numbers | not set | A projected view's latitudes as `[south, north]`, in degrees within ±90, beside `lon`, with `north` not below `south`. |

## `[view.point_visibility]`

Where each point's access label comes from. A viewer sees a point when they hold one of its labels. Write `field` or `source`, not both, and at least one of the three keys.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `field` | string | not set | A column of the view's points file holding each point's access label, as a string or a list of strings. A null or an empty list is no label. An empty name is refused. |
| `source` | string | not set | A name in `[sources]`: a file of integer `entity_id` and `term_id` columns, one row per point and access term. If one view of a build reads labels this way, every view must, from the same file. |
| `default` | string | not set | The label a point with none of its own takes: `public` for every viewer, or an access label the plugin maps to a term. `inherited` is refused. Without it, a point with no label is refused, at a build and at `/control/ingest` alike. |

## `[[view_group]]`

A set of views that share every setting and differ by a key, and by the metadata each view carries. A view of the group is addressed `<group>:<key>`. The roster, which says what the views are, takes one of three forms:

- `[[view_group.view]]` blocks, one per view, each naming its own points file. The group names no `source`.
- A `[view_group.views]` file with one row per view, beside a group `source` holding every view's points in rows that name their view in a `view` column.
- Neither: the group's `source` holds every view's points, and the views are the distinct values of its `view` column. They carry no metadata.

Writing both rosters is refused. `[defaults].source` does not reach a group, so a build refuses a group with neither a `source` nor `[[view_group.view]]` blocks.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The group's name, the first half of each of its views' ids. ASCII letters, digits, `_` and `-`, unique among views and view groups. |
| `title` | string | not set | A display title, served on `/v1/meta`. |
| `projection` | string | `"none"` | As on `[[view]]`. |
| `source` | string | not set | A name in `[sources]`: the file holding every view's points, one row per item and view. Required with `[view_group.views]` and refused beside `[[view_group.view]]` blocks. |
| `fields` | table of strings | not set | Where the group's `source` keeps each field: as on `[[view]]`, and `view`, the column naming each row's view. Refused on a group with no `source`. |
| `extent` | string or table | required | As on `[[view]]`: one frame for every view of the group. |
| `point_visibility` | table | required | As on `[[view]]`, for every view of the group. |
| `visibility` | string or array of strings | `"public"` | As on `[[view]]`, for every view of the group. A view of the group may narrow it with a `visibility` of its own. |
| `members` | string | not set | Another group's name: this group's views are that group's, and its own `source` holds their points. The group named may not itself name `members`, and a roster or `metadata` beside `members` is refused. |
| `metadata` | table | not set | The values each view carries, served with it on `/v1/meta`: `name = "type"` over the `[[attribute]]` types, or `name = { type = "category", vocabulary = "<name>" }`. Every view must carry every name. `key`, `source`, `visibility` and the group's `view` column are refused as names, and so is `metadata` on a group with no roster or one that names `members`. |
| `view` | array of tables | `[]` | The roster as one block per view: keys under `[[view_group.view]]`. |
| `views` | table | not set | The roster as a file: keys under `[view_group.views]`. |

## `[[view_group.view]]`

One view of a group, written in the declaration. Beside these keys the block takes one for each name the group's `metadata` declares, holding a value of the declared type, and every view carries every name. A `timestamp_us` value is an offset date-time, such as `2026-04-01T00:00:00Z`, or microseconds since the Unix epoch; an integer outside its type's range is refused. A group-level key, such as `extent` or `projection`, is refused, and so is any other key.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | string | required | The view's key: its id is `<group>:<key>`. ASCII letters, digits, `_` and `-`, and no two views of the group may share one. |
| `source` | string | not set | A name in `[sources]`: the file holding this view's points. A build refuses a view with none. |
| `visibility` | string or array of strings | not set | The label, or list of labels of which a viewer must hold one, to reach this view as well as the group. Without it, or with `public` alone, the group's `visibility` is the view's only gate. |

## `[view_group.views]`

The roster as a file of one row per view, beside the group's `source`. Its columns are `key`, `visibility`, and one for each `metadata` name.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `source` | string | required | A name in `[sources]`: the roster file. |
| `fields` | table of strings | not set | Where the roster file keeps `key`, `visibility` and each metadata name, as `field = "column"`. |

## `[[vocabulary]]`

A named set of values that `category` attributes and category metadata draw on. Each value is stored as an integer code: pinned where the declaration or its file writes one, and otherwise drawn at random, which a rebuild does again.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The vocabulary's name. ASCII letters, digits, `_` and `-`, unique among vocabularies. |
| `title` | string | not set | A display title. Not built yet: the title is accepted and not published. |
| `width` | string | required | The width codes are stored at: `u8`, `u16` or `u32`, whose largest codes are 255, 65,535 and 4,294,967,295. |
| `value_set` | string | required | `closed` refuses a value the vocabulary does not hold, at a build and at ingest. `open` gives such a value a new code. |
| `visibility` | string | required | `public` shows every viewer every value. `derived` shows each viewer only the values carried by points they can see. |
| `source` | string | not set | A name in `[sources]`: a file of values, with a `key` column and optional `code` and `title` columns. A value with no code is drawn one. Refused beside `values`. |
| `fields` | table of strings | not set | Where the values file keeps `key`, `code` and `title`, as `field = "column"`. |
| `values` | array of strings, or table of integers | not set | The values written here: an array of keys, each drawn a code, or a table of `key = code`, which pins them. A code is from 1 to the width's largest, since 0 means no value. Two values at one code, a code also `reserved`, an empty key and a key given twice are refused, and so is `values` beside `source`. |
| `reserved` | array of integers | not set | Retired codes, from 1 to the width's largest, which no value may hold and no draw picks. |

## `[[attribute]]`

A column each item carries. `render` and `index` decide where its value is kept, and may be set together. `render = true` stores the value beside each point's position, so it travels with every point a viewport returns and can colour the map. `index = true` builds an index that filters and searches by it. With neither, the value is kept in the item's record and read when the item is opened.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The column's name, which filters and `/v1/categories/{column}` use. ASCII letters, digits, `_` and `-`, unique among attributes. `tessera_id`, `residual`, `external_id`, `x`, `y`, `access`, `node_id`, `record`, `all_of`, `any_of`, `none_of`, `region`, `member_of` and `highlighted` are refused. |
| `title` | string | not set | A display title. Not built yet: the title is accepted and not published. |
| `field` | string | the attribute's `name` | The column of the source file holding the values. An empty name is refused. |
| `source` | string | the value of `[defaults].source` | A name in `[sources]`: the file the values are read from, joined to the points by entity id. A build refuses an entity-scoped attribute with no source here or in `[defaults]`. A group-scoped attribute with none reads each view's own points file, and `[defaults]` does not reach it. |
| `entity_id_field` | string | the value of `[defaults].entity_id_field` | The column of `source` holding the entity id. An empty name is refused. |
| `type` | string | required | The type of value the column holds, one of the types below. |
| `vocabulary` | string | not set | The `[[vocabulary]]` a `category` draws on. Required on a `category`, refused on every other type, and a name no `[[vocabulary]]` declares is refused. |
| `render` | boolean | `false` | Store the value beside each point's position. Refused on `keyword` and `text`. |
| `index` | boolean | `false` | Build an index that filters and searches by the value. A group-scoped `text` column needs it. |
| `unique` | boolean | `false` | No two items may hold one value. The build refuses a source holding one value twice, naming how many values and up to ten of them, and an ingest giving an item a value another item holds is refused. `eq` and `in` filters on the column are answered from its index, with or without `index = true`; without it, they are the only filters it takes. Applies to `keyword`, integer and `timestamp_us` columns scoped to the entity. A null is no value, so any number of items may hold one. |
| `scope` | string or table | `"entity"` | `"entity"`: one value per item, the same under every view. `{ group = "<name>" }`: one value per item and view of that group, which must own its views rather than name `members`. |
| `fields` | table of strings | not set | On a group-scoped attribute with a `source` of its own, `view` names the column saying which view each row's value is for, `view` if absent. Refused on any other attribute, and any other field is refused. |
| `analyser` | string | `"unicode"` | The analyser that turns a `text` column into search terms. This build has `unicode`, and refuses any other name. Refused on every other type. |

The types `type` takes:

| Type | Holds |
| --- | --- |
| `bool` | `true` or `false` |
| `u8` | an integer from 0 to 255 |
| `u16` | an integer from 0 to 65,535 |
| `u32` | an integer from 0 to 4,294,967,295 |
| `u64` | an integer from 0 to 2^64 - 1 |
| `i8` | an integer from -128 to 127 |
| `i16` | an integer from -32,768 to 32,767 |
| `i32` | a signed 32-bit integer |
| `i64` | a signed 64-bit integer |
| `f32` | a 32-bit floating-point number |
| `f64` | a 64-bit floating-point number |
| `timestamp_us` | an instant, stored as microseconds since the Unix epoch |
| `keyword` | a short string matched exactly, such as an identifier or a hostname. Refused with `render = true` |
| `text` | prose, searched by the terms its `analyser` produces. Refused with `render = true` |
| `category` | a value of the vocabulary `vocabulary` names, stored as its code at the vocabulary's `width` |

## `[[layer]]`

An annotation layer: a named set of artifacts, such as clusters or regions, drawn over one or more views. An artifact's members are the items it contains. Three keys decide who may learn that an artifact exists, and none has a default: `visibility`, `artifact_visibility` and `require_member_visibility`.

A layer's artifacts come from its own `source` file, from `[[layer.artifacts]]` blocks, or from neither, in which case the layer is declared empty and filled through the control plane. `[defaults].source` does not reach a layer.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The layer's name. Unique among layers, not empty, and not `all` in any case, which a viewport request writes for every layer. |
| `title` | string | not set | A display title, served on `/v1/meta`. |
| `views` | array of strings | required | The views the layer is drawn on. A view group's name draws it on every view of the group. A name no `[[view]]` or `[[view_group]]` declares, and a name given twice, are refused. |
| `scope` | string or table | `"entity"` | `"entity"`: one set of artifacts drawn on every view the layer names. `{ group = "<name>" }`: a set for each view of that group, each artifact naming its view in the `view` field, and `views` may name only that group's views. |
| `source` | string | not set | A name in `[sources]`: the artifacts file, one row per artifact. Refused beside `[[layer.artifacts]]`. |
| `fields` | table of strings | not set | Where the artifacts file keeps each field, as `field = "column"`. The fields are `key`; `members` or `excluding`, on an `enumerated` layer; `contents`, where the layer supplies content; `parent`, under a `nested`, `dag` or `tiered` hierarchy; `attached_layer` and `attached_key`, where it has `depends_on`; `view`, on a group-scoped layer; `space`, where it has a shape or `polygon` content; and the shape's own fields: `min_x`, `min_y`, `max_x` and `max_y` for a `bbox`, `cx`, `cy` and `r` for a `circle`, `cx`, `cy`, `a`, `b` and `angle` for an `ellipse`, and `geometry`, as WKB, for a `polygon`. `level` and `attached_level` are read under their own names. A field the layer does not declare, both `members` and `excluding`, and `fields` beside `[[layer.artifacts]]` are refused. |
| `artifacts` | array of tables | not set | Artifacts written in the declaration: keys under `[[layer.artifacts]]`. Refused beside `source`. |
| `membership` | string or table | required | How an artifact's members are found. `"enumerated"`: a stored set per artifact. `"spatial"`: the points inside the artifact's shape. `{ attribute = "<name>" }`: one artifact per value of that attribute, which must have `index = true` and a `category`, `u8`, `u16` or `u32` type. |
| `members` | table | not set | Members as a file of their own, one row per artifact and member: keys under `[layer.members]`. Only on an `enumerated` layer, and refused beside a `members` or `excluding` field. |
| `value_set` | string | `"closed"` | `"closed"`: the layer's artifacts are the ones its `source` or `[[layer.artifacts]]` declare, and a member row naming another is refused. `"open"`: a member row naming a new key creates an artifact with that key. Any other value is refused. |
| `hierarchy` | table | required | How the layer's artifacts relate to each other: keys under `[layer.hierarchy]`. |
| `levels` | array of tables | `[]` | The layer's levels: keys under `[[layer.levels]]`. Required under a `stacked` or `tiered` hierarchy, and refused under `nested` and `dag` and on an attribute membership. |
| `layout` | string | chosen by the server, again at each compaction | How the layer is stored for serving. `"rows"`: a set of rows per artifact. `"column"`: one artifact per row, for a level whose artifacts do not overlap. `"list"`: a list of artifacts per row. Refused on an attribute membership. |
| `visibility` | string | required | The access label a viewer must hold to learn that the layer exists, or `public` for every viewer. An empty label and `inherited` are refused. |
| `artifact_visibility` | table | required | What gates each artifact beyond `visibility`: keys under `[layer.artifact_visibility]`. |
| `require_member_visibility` | string or table | required | How much of an artifact's membership a viewer must see for the artifact to be served: `"all"`, `"any"`, `{ count = n }` with n at least 1, `{ fraction = p }` with p a float above 0 and at most 1, or `"none"` for no such rule. `"all"` and `fraction` are refused on a `spatial` or attribute membership. |
| `withdraw_on_member_deletion` | boolean | `false` | Only `false` is accepted. Not built yet: withdrawing an artifact when one of its members is deleted. A deleted member leaves the artifact, and its computed content is recomputed from the members left. |
| `depends_on` | array of strings | `[]` | Layers whose artifacts this layer's artifacts attach to, through `attached_layer` and `attached_key`. Each must be declared before this one. Naming the layer itself is refused, and so is `depends_on` on an attribute membership. |
| `content` | table | not set | What each artifact carries besides its members: keys under `[layer.content]`. |
| `shape` | table | not set | The kind of shape each artifact of a `spatial` layer carries: keys under `[layer.shape]`. Refused on any other membership. A `spatial` layer without one holds no artifacts. |
| `default_space` | string | `"view"` | The space a row of the `source` file writes its geometry in when it names none: `"view"`, the view's own coordinates, or `"wgs84"`, longitude and latitude in degrees, which every view the layer is drawn on must have a projection to take. Refused on a layer with neither a shape nor `polygon` content. A `[[layer.artifacts]]` row without a `space` is in `"view"` whatever this says. |
| `labels` | table | not set | A layer of labels for this layer's artifacts, written here: keys under `[layer.labels]`. |

## `[[layer.artifacts]]`

One artifact written in the declaration, for a layer a person authors rather than a pipeline produces. Its keys are the artifacts file's fields under their own names, and a layer written this way builds the same bundle as the same rows in a file. A shape is written in the key its layer's `[layer.shape]` kind names.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `key` | string | required | The artifact's key, which `parent` and `attached_key` name it by. |
| `level` | integer | `0` | The level the artifact is at: 0 on a layer with no levels. |
| `members` | array of integers | not set | The members, as the integer values of the view's `entity_id` field. Only on an `enumerated` layer, and refused beside `excluding`. |
| `excluding` | array of integers | not set | The members by exclusion: the ids of the points the artifact leaves out, which the build turns into `members`. Only on an `enumerated` layer, and refused beside `members`. |
| `contents` | array of arrays of strings | `[]` | The artifact's content, best first: one array per rank, holding a value for each `[[layer.content.supplied]]` entry in order. |
| `bbox` | array of numbers | not set | A `bbox` layer's shape: `[min_x, min_y, max_x, max_y]`. |
| `circle` | array of numbers | not set | A `circle` layer's shape: `[cx, cy, r]`. |
| `ellipse` | array of numbers | not set | An `ellipse` layer's shape: `[cx, cy, a, b, angle]`, with `a` and `b` its semi-axes and `angle` its rotation in degrees. |
| `wkt` | string | not set | A `polygon` layer's shape, as well-known text: `"POLYGON ((…))"`. |
| `space` | string | `"view"` | The space the row's geometry is written in: `"view"` or `"wgs84"`, which every view the layer is drawn on must have a projection to take. It governs the shape and any `polygon` content. |
| `parent` | string or array of strings | `[]` | The artifact's parents, by key: one under a `nested` or `tiered` hierarchy, and any number under `dag`. |
| `attached_layer` | string | not set | The layer this artifact attaches to, one that `depends_on` names. |
| `attached_level` | integer | `0` | The level of the artifact this one attaches to. |
| `attached_key` | string | not set | The key of the artifact this one attaches to. |
| `access` | string or array of strings | not set | The artifact's own access labels, on a layer whose `artifact_visibility` names a `field`: one label, a list, or `[]` for none of its own. Such a layer refuses a row without it. |

## `[layer.members]`

An `enumerated` layer's members as a file of their own, one row per artifact and member, for a membership too large for one field of the artifacts file. Under `value_set = "closed"`, a members file needs the layer's own `source` or `[[layer.artifacts]]` for its rows to name.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `source` | string | not set | A name in `[sources]`: the members file. |
| `fields` | table of strings | not set | Where the members file keeps each field, as `field = "column"`: `key`, the artifact's key; `entity`, the member's id; and `rank`, where the layer supplies content, the rank in `contents` whose content was made from this member. |

## `[layer.hierarchy]`

How a layer's artifacts relate to each other, written as `hierarchy = { kind = "flat" }`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `kind` | string | required | `flat`: one population. `nested`: a tree, each artifact naming its `parent`, every artifact at level 0. `dag`: the same with any number of parents. `stacked`: independent analyses, one per level. `tiered`: levels, each artifact naming a `parent` at a coarser level. An attribute membership takes `flat` alone. |
| `prune_children` | boolean | `false` | Serve only the deepest artifact that passes along each branch. |

## `[[layer.levels]]`

One level of a `stacked` or `tiered` layer. Levels are numbered from 0 with none repeated or missing.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `level` | integer | required | The level's number, which an artifact names in its `level`. |
| `title` | string | not set | A display title, served on `/v1/meta`. |
| `zoom` | array of 2 integers | every zoom | The zoom levels the level is served at, `[low, high]` inclusive. A request naming no levels is answered with the levels whose range covers its zoom. A range with `low` above `high`, or starting past 16, is refused. |

## `[layer.artifact_visibility]`

What gates each artifact beyond the layer's own `visibility`. Written as `artifact_visibility = { default = "inherited" }`, or with a `field` naming each artifact's own labels.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `field` | string | not set | The field holding each artifact's own access labels: a column of the artifacts file, or `access` on a `[[layer.artifacts]]` row. Naming it declares that artifacts carry labels of their own. An empty name is refused, and so is `field` on an attribute membership. |
| `default` | string | required | The label an artifact with none of its own takes: an access label, `public` for every viewer, or `inherited`, which leaves the layer's `visibility` as the artifact's only gate. An empty label is refused. |

## `[layer.content]`

What each artifact carries besides its members.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `computed` | array of strings | `[]` | Properties the server computes from the members each viewer can see: `centroid`, `box` and `hull`. Any other name, a name given twice, and `computed` on an attribute membership are refused, and so is `hull` beside a shape or `polygon` content, since an artifact draws one shape. |
| `supplied` | array of tables | `[]` | Content the artifacts carry, one entry per kind: keys under `[[layer.content.supplied]]`. |

## `[[layer.content.supplied]]`

One kind of content each artifact carries, in its `contents`. Refused on an attribute membership.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The content's name, unique within the layer. |
| `type` | string | required | `text`, `polygon`, `extent` or `point`, published on `/v1/meta` so a client knows how to draw it. `polygon` content is the artifact's drawn shape, so it is refused beside a `hull` or a `[layer.shape]`. |
| `require_member_visibility` | string | required | `"all"`: content made from the members, served only to a viewer who can see every member it was made from. `"inherited"`: content true whatever the members, such as a name a person wrote, gated by the artifact alone. |

## `[layer.shape]`

The kind of shape each artifact of a `spatial` layer carries. An artifact's members are the points inside its shape.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `kind` | string | required | `bbox`, `circle`, `ellipse` or `polygon`. |
| `depth` | any | not set | Refused: every shape kind is exact, so a shape has no depth. |

## `[layer.labels]`

A layer of labels for the artifacts of the layer it is written in, such as a name for each cluster. It builds the same bundle as a second `[[layer]]` written after this one, with the same `views` and `scope`, `hierarchy = { kind = "flat" }`, `depends_on` naming this layer, and one `[[layer.content.supplied]]` entry named for the label layer, of type `type`, at the requirement `[layer.labels.content]` states. Each label names the artifact it labels with `attached_layer` and `attached_key`. A label layer that needs any other key is written out as a `[[layer]]`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `name` | string | required | The label layer's name, as on `[[layer]]`. |
| `title` | string | not set | A display title, served on `/v1/meta`. |
| `source` | string | not set | As on `[[layer]]`: the labels file, one row per label. |
| `fields` | table of strings | not set | As on `[[layer]]`. |
| `members` | table | not set | As `[layer.members]`: the members each label was made from. |
| `type` | string | required | The kind of content each label is: `text`, `polygon`, `extent` or `point`. |
| `membership` | string or table | required | As on `[[layer]]`. |
| `require_member_visibility` | string or table | required | As on `[[layer]]`: how much of a label's membership a viewer must see for the label to be served. |
| `content` | table | required | The requirement on each label's content: keys under `[layer.labels.content]`. |
| `artifact_visibility` | table | required | As on `[[layer]]`. |
| `visibility` | string | the `visibility` of the layer it is written in | As on `[[layer]]`. |

## `[layer.labels.content]`

The requirement on each label's content, which becomes its content entry's `require_member_visibility`.

| Key | Type | Default | Description |
| --- | --- | --- | --- |
| `require_member_visibility` | string | required | `"all"` where the label was made from the items it names, so a viewer reads it only when they can see all of them. `"inherited"` where it is true whatever they are, such as a name a person wrote. |
