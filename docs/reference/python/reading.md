# Reading

A reader holds a set of access terms and sees an item when its terms satisfy one of the item's labels. Every count, map, record and annotation a reader is given is computed over the items it may see. A `Database` reads with its operator's own token, which holds `read-all` and sees every item that is not deleted or suppressed, including an item committed under a new term or label after the token was made, once the commit is flushed. `Database.viewer` gives a `Viewer` holding the terms you name, and `connect` one holding the terms its token grants.

## Database

::: mosaica._database.Database.view
    options:
      heading_level: 3

::: mosaica._database.Database.viewer
    options:
      heading_level: 3

::: mosaica._database.Database.map
    options:
      heading_level: 3

::: mosaica._database.Database.meta
    options:
      heading_level: 3

::: mosaica._database.Database.item
    options:
      heading_level: 3

::: mosaica._database.Database.items
    options:
      heading_level: 3

::: mosaica._database.Database.lookup
    options:
      heading_level: 3

::: mosaica._database.Database.artifacts
    options:
      heading_level: 3

::: mosaica._database.Database.viewport_artifacts
    options:
      heading_level: 3

::: mosaica._database.Database.aggregate
    options:
      heading_level: 3

::: mosaica._database.Database.categories
    options:
      heading_level: 3

::: mosaica._viewer.Viewer
    options:
      merge_init_into_class: true

::: mosaica._viewer.Selection

::: mosaica._viewer.Sample

::: mosaica._viewer.Batches

::: mosaica._viewer.PartialRead
