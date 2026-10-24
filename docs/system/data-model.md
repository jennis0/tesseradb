# Data model

A Mosaica database is built from four kinds of thing:

- **Items**: the records you want to map, each with its own field values.
- **Views**: layouts that give each item a position on a map.
- **Annotation layers**: structure drawn over the items, such as clusters, topics or regions. Each cluster, topic or region in a layer is called an artifact.
- **Vocabularies**: the lists of values that category fields take.

This chapter follows one example through all four: a collection of research papers, laid out by what their abstracts say, with a hierarchy of topics drawn over them.

```mermaid
flowchart LR
  subgraph items["Items"]
    p["a paper<br/>title, year, archive, abstract"]
  end
  subgraph views["Views"]
    m["map<br/>a layout of every paper"]
    y["by_year<br/>one layout per year"]
  end
  subgraph layers["Annotation layers"]
    t["topics<br/>field → subfield → topic"]
  end
  p --> m
  p --> y
  t -. "artifacts over the papers" .-> m
```

*Each paper is one item. Views place it on maps. A layer groups papers into topics drawn over a view.*

??? info "How does Mosaica compare to a relational database?"

    | Mosaica | Nearest relational idea |
    |---|---|
    | Item | A row of a main table, keyed by its `mosaica_id` or by a unique field |
    | Field | A column of that table |
    | View | A child table of positions, one row per item placed in it, stored in order of position |
    | View group | A table partitioned by key, with partitions added and dropped while running |
    | Annotation layer | A table of artifacts, a table joining each artifact to its member items, and a parent column for hierarchies |
    | Vocabulary | A lookup table that a category column refers to |
    | Access label | A row-level security policy |

    A relational database is built to answer any query, planned when you ask it. Mosaica is built to answer one question quickly over billions of rows: what is in this map tile, for this viewer, under these filters. That is why it differs.

    - **Each layout is stored in its own order.** A tile is quick to read when its points are stored together, but a table can be stored in only one order, and two layouts put the same item in different places. So each view is its own table, ordered by position, and what is true of the item everywhere is kept apart from both.
    - **Drawn columns sit beside the positions.** A rendered field is copied next to each position, so drawing a tile reads one place. Since that decides how the view is stored, `render` is fixed once declared.
    - **Annotations are built in.** In SQL, counting the papers a viewer may see in each topic on screen is a join and a grouping on every request. Mosaica stores each artifact's members so that they can be intersected directly with what the viewer may see, which keeps those counts quick enough for a map.
    - **Joins happen when data is written.** Mosaica takes no ad hoc queries and joins nothing when you ask. Rows from different files are tied to their items once, when they are inserted, by a unique field or a `mosaica_id`. That is why a unique field matters.

## Items and fields

An item is one record: in the example, one paper. Its data is held in fields, one for each column you declare, such as `title`, `year`, `archive` and `abstract`.

Each field has a type:

| Kind | For example | A viewer can filter it by |
|---|---|---|
| Number | `year`, `citations` | an exact value, a set of values, or a range |
| True or false | `open_access` | the same as numbers |
| Date and time | `published` | the same as numbers |
| Category | `archive`, from a fixed list of archives | a value or a set of values, and can pick values from a list |
| Keyword | `doi` | an exact value, a set, a prefix, or a substring |
| Text | `abstract` | the words it contains |

For each field you also decide how it is used. There are two choices, and a field can take both, either or neither.

- **Rendered.** The value is sent with every point drawn, so the map can colour or size points by it. Render `archive` to colour the map by archive. Every rendered field makes each point larger, so render only what the map needs. Keywords and text cannot be rendered.
- **Indexed.** The field can be filtered, searched and counted. Index `year` to filter by a range of years, and `abstract` to search it. An index takes disc space and time to build.

A field that is neither is still stored. It is shown when someone opens the item, and returned when items are read in bulk. Use this for anything a viewer reads but never filters on, such as the full author list.

### Naming an item

Mosaica gives every item a `mosaica_id`, and every response that returns an item includes it. It stays the same for the item's whole life in the database, through edits and restarts.

Your data usually has its own identifier too, such as a DOI. Declare that field `unique`, and you can name items by it wherever Mosaica asks which item you mean: to edit one, delete it, add it to a view, or put it in a cluster.

!!! note "Declare a unique field if you will ever update your data"
    A `mosaica_id` changes when you rebuild the database from files. A unique field of your own does not, so updates, deletions and annotation files that name items by it keep working. Without a unique field, each row of a view's points file becomes a new item.

## Views

A view lays the items out on a map, giving each one a position. The example has a view called `map`, where each paper's position comes from a UMAP projection of its abstract's embedding, so papers on similar subjects sit together.

There are two kinds of view:

- **Plain views** take any x and y, such as the output of UMAP or t-SNE, and draw them as they are.
- **Geographic views** take longitude and latitude in degrees, and draw them with a map projection: `web_mercator`, the projection web maps use, or `equirectangular` or `gall_isographic`, which reach the poles.

A view's **frame** is the area its positions are stored across. Most declarations use `auto`, which fits the frame to the data when the database is first built.

!!! note "Points outside the frame"
    A point outside the frame is drawn on its edge, and the build's report or the insert's answer counts how many were. A build refuses a frame that more than half of the points fall outside. A view added to a running server has no data to fit, so it needs an explicit frame.

### Several views of the same items

The same papers can have more than one view: one laid out by abstract, another by citation graph, say. Each view gives the paper its own position. Everything else about the paper, its fields, its access label and its `mosaica_id`, is shared, so a viewer who switches from one view to another keeps their filters.

An item does not have to be in every view. A view holds the items that were given a position in it.

### View groups

Sometimes you want many views of the same kind. In the example, you might want a separate layout for each year, so that a viewer can see how the subjects of one year's papers relate, and add next year's layout when it arrives.

A view group is a family of views like this. Each view in the group has its own key, such as `2024`, and is named `by_year:2024`. Each holds its own items and positions. They share every other setting: projection, frame and who may see them.

Views in a group can be created and dropped while the server runs, without a rebuild. A group can also carry facts about each view, such as the date range a year covers, and a field can hold a different value for each view, such as a score recomputed every year.

!!! warning "Dropping a view deletes items that are in no other view"
    When a view of a group is dropped, any item that has a position in no other view is deleted, as if you had deleted it yourself. The answer to the drop says how many. Items that are also in another view are not affected.

## Annotation layers

An annotation layer adds structure on top of the items. In the example, the `topics` layer groups papers into fields, subfields and topics. Each field, subfield and topic is an **artifact**: a named thing with a set of member items.

An artifact can carry:

- a **title**, such as "Graph Neural Networks";
- a **count** of its members, which Mosaica works out for each viewer from the members that viewer may see;
- a **centre**, a **bounding box** and a **hull** around its members, which Mosaica computes;
- a **shape** you supply, such as a country's border.

A layer finds each artifact's members in one of three ways:

| Membership | Members are | Example |
|---|---|---|
| Listed | the items a table names for each artifact | the output of a clustering run, one row per paper and cluster |
| By shape | the items whose positions fall inside the artifact's shape | country borders on a world map |
| By a category field | the items carrying each value of the field | one artifact per archive |

A layer can be flat, a single set of artifacts, or arranged in levels. In a hierarchy, each artifact names its parent, as each topic names its subfield. A topic can have more than one parent, if the layer is declared as one that allows it. A viewer can choose which level to show and colour the map by it.

A layer is drawn on the views you name. A layer can also be declared for a view group, with its own set of artifacts for each view, such as a separate clustering for each year.

Each layer has its own access label, and can gate each artifact further, so a viewer may see the papers but not a layer of internal labels over them. The [annotations](annotations.md) chapter covers all of this in detail.

## Vocabularies

A category field takes its values from a vocabulary, a named list of values. In the example, `archive` uses a vocabulary of arXiv archives: `cs`, `math`, `astro-ph` and so on, each with an optional title such as "Computer Science". Several fields can share one vocabulary.

A vocabulary has three settings:

- **Size**: how many values it can hold, up to 255, 65,535 or about 4.3 billion. This is fixed once declared.
- **Closed or open**: a closed vocabulary refuses a value it does not hold. An open one adds it.
- **Public or derived**: a public vocabulary lists every value to every viewer. A derived one lists to each viewer only the values carried by items that viewer may see. Use derived when the list of values would itself reveal something, such as the names of clients.

## What can change while it runs

| | At a running server |
|---|---|
| Items | Insert, edit, delete, suppress and lift a suppression |
| Fields | Declare a new field. Its type, `render` and `index` are fixed once declared. `unique` can be turned on or off |
| Plain views | Declare a new view. A view cannot be changed or dropped |
| View groups | Declare a group, and create and drop its views. A group's view cannot be changed; drop it and create it again |
| Annotation layers | Declare a layer, publish artifacts into it, and drop it |
| Vocabularies | Declare one, add values, and change values' titles |

Anything else needs a rebuild from files.

!!! note "Turning `unique` on for an existing field"
    Mosaica builds the index over every stored value. If any value is held by more than one item, the change is refused with a count and up to ten of the values, and the field stays as it was. Inserts and deletions carry on while the index is built.

## Not built yet

- **Fields holding a list of values.** A field holds at most one value per item.
- **Declaring a rendered field at a running server.** Declare it at a build.
- **Dropping a plain view.** A view in a view group can be dropped.
- **Titles of views, fields and vocabularies.** A declaration may give one, but it is not yet sent to clients. Titles of category values and of layers are sent.
