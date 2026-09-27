# Declaring

Declaring says what a database holds: its views, columns, vocabularies and annotation layers. A declaration takes no data; [`insert`](inserting.md) hands a table to something declared. Each `declare_*` method adds to the declaration file, `schema.toml`, which the next `check()` or `commit()` checks.

## Database

::: tesseradb._database.Database.declare_view
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_view_group
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_attribute
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_unique
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_join_field
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_vocabulary
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_columns
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_layer
    options:
      heading_level: 3

::: tesseradb._database.Database.declare_labels
    options:
      heading_level: 3

::: tesseradb._database.Database.declare
    options:
      heading_level: 3

::: tesseradb._database.Database.declaration
    options:
      heading_level: 3

::: tesseradb._database.Database.write
    options:
      heading_level: 3

::: tesseradb._reports.Declared
    options:
      show_bases: false

::: tesseradb._columns.DeclaredColumn
