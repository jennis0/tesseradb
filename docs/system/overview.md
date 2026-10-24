# What Mosaica does

Mosaica serves interactive maps of large sets of records, each placed at a point. People pan, zoom, filter and search the map in a browser or a notebook, and every count, cluster and sample they see is worked out for the view in front of them. The records can be added to, edited and deleted while the map is in use.

You would reach for it when there are too many points for a plotting library, or when the data changes too often to bake into map tiles. It also suits data that different people may see different parts of.

## Your data

- **Items.** Each record is an item: a place, a paper, a specimen, a log event. An item has fields, and a position in one or more views.
- **Views.** A view lays the items out on a map. It can be geographic, with longitude and latitude drawn in a map projection, or any two coordinates, such as a UMAP or t-SNE projection of embeddings.
- **Several views of the same items.** One set of items can have several views, such as a world map and an embedding. An item keeps its identity across them, so a viewer can switch from one to the other and keep their selection. A view group is a family of views that share their settings, such as one per year.
- **Fields.** Each column of your data becomes a field: a number, a date and time, a category, a keyword or free text. For each field you choose how it is used:
    - *rendered*: sent with every point drawn, so the map can colour or size points by it;
    - *indexed*: filterable, searchable and countable;
    - *neither*: stored, and shown when someone opens the item. This costs least.

The [data model](data-model.md) chapter covers views, fields and identifiers in full.

## On the map

A viewer on the map can:

- pan and zoom, with a count of what is in view, and density shading where there are too many points to draw;
- colour points by a category or a cluster, and size them by a number;
- filter by ranges of numbers and dates, by category values, and by exact, prefix or substring matches on keywords;
- search free text by word;
- draw a box, circle, ellipse or polygon to select an area;
- combine any of these filters with and, or and not;
- highlight the items that match a test, without hiding the rest;
- open an item to see all of its fields;
- switch between views and keep the same filters.

Every count follows the filters. When a viewer zooms in, the points already drawn stay, and more appear.

The [queries](queries.md) chapter covers each kind of filter and how the points on screen are chosen.

## Annotations

Annotation layers add structure over the points. An annotation can be:

- a cluster, from any clustering you have run;
- a node in a hierarchy, such as field, subfield and topic, or kingdom down to species. A node can have more than one parent;
- a region with a shape, such as a country border or a drawn area. Its members can be the points inside the shape;
- a label attached to another annotation.

Each annotation has a title and a count of its members, and can carry its centre, its bounding box and a hull around its points. A viewer can colour the map by a layer, choose which level of a hierarchy to show, and filter to the members of chosen annotations. Counts follow the filters, as they do for points.

The [annotations](annotations.md) chapter covers declaring and publishing layers.

## Counts and reads

Beyond the map, you can ask for numbers and records directly:

- **Breakdowns.** Count the matching items by the values of a category, or by the annotations of a layer, top values first.
- **Comparisons.** Compare a set with a reference set, such as a filtered set with everything, and get each group's count in both and how much more common it is in one than the other.
- **Density.** Count the matching items in a grid of cells at any depth.
- **Records in bulk.** Read every matching item, or every annotation, page by page as Apache Arrow tables.

## Changing data while it serves

A running server accepts these writes:

- **Insert** new items, and add existing items to a further view.
- **Edit** an existing item's fields or position.
- **Delete** an item. This is permanent.
- **Suppress** an item to hide it, and **lift** the suppression later.
- **Declare** new views, fields and annotation layers, and publish annotations into them.

A deletion or suppression applies to every request that starts after the server accepts it. New items and edits appear at the next flush, which runs every 90 seconds by default, or sooner once 40,000 rows are waiting. A write can ask to be answered only when it is visible. The server records each write in a log before answering it, so an accepted write survives a restart.

Anything you can declare when you first build a database, you can also declare to a running server, and the reverse.

The [write path](write-path.md) chapter covers ingest, flushes and compaction.

## Who sees what

Mosaica can give each viewer a map computed only from the items they may see.

- **Access labels.** Any item can carry a label, an expression over terms such as `secret&(team_a|team_b)`.
- **Viewers.** A viewer signs in with a password, an API key, or a token from an OpenID Connect provider. Alternatively, your own backend starts a session on their behalf. Either way, the session holds a set of terms, and the viewer may see the items whose labels those terms satisfy.
- **Everything follows.** Every count, sample, density, cluster count and annotation title is computed from the items the viewer may see. A filter only narrows that set. A viewer who may see few items still gets a full sample of those.
- **Public data.** Data with no labels needs no set-up. Every item gets the label `public`, which every viewer holds.

The [access control](access-control.md) chapter covers labels and sign-in, and the [security](security.md) chapter sets out what a viewer can and cannot learn.

## Ways to work with it

| | Declare and write | Map | Counts and records | Manage users |
|---|---|---|---|---|
| Python client | Yes | A widget in Jupyter | Yes, as Arrow tables | Yes |
| TypeScript client | Yes | Web components, or a deck.gl layer | Yes | Yes |
| `mosaica` command line | Builds from files. Writing to a running server is not built yet | Not built yet | Yes | Yes |
| HTTP API | Yes | Yes, as tiles of points | Yes | Yes |

The [capabilities](../reference/capabilities.md) table names the call for each capability on each surface.

## Why it is fast

- **A tile is one run of rows.** The build stores each view's items in Morton order, which keeps points that are near each other on the map near each other on disc. Every map tile, at every zoom, is then one contiguous run of rows.
- **Counts read no rows.** Filters and access are held as compressed bitmaps of row numbers. A tile's count is the size of an intersection between bitmaps and a row range.
- **One machine.** The build streams its input, so the source files need not fit in memory, and the server runs within a memory cap you set. The [operating guide](../guides/operating.md) explains how to size it.

The [queries](queries.md) and [serving](serving.md) chapters explain these in full.

## What is built

| Feature | Status |
|---|---|
| Build, serve, live ingest, deletion and suppression | Built |
| Filters and search over numbers, dates, categories, keywords and text | Built. A field holds one value per item; fields holding a list of values are not built yet |
| Several views over one set of items, and view groups | Built |
| Annotation layers: clusters, hierarchies, regions and hulls | Built |
| Reading records and annotations in bulk | Built, on all four surfaces. A read pinned to one version of the data is not built yet |
| Writing to a running server, and drawing a map, from the command line | Not built yet |
| Python and TypeScript packages | Built. Not yet published to PyPI or npm |
| A conformance suite that checks a running server's answers against an independent reference implementation | Built |
| Running across several machines, and replicas | Not built |
