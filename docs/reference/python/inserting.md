# Inserting and committing

`insert` hands a table to something declared and names the columns it reads. `commit` makes what was inserted part of the database: the first commit builds it and starts its server, and each later one sends what was inserted since. `check` says what the next commit would do, and sends nothing.

## Database

::: tesseradb._database.Database.insert
    options:
      heading_level: 3

::: tesseradb._database.Database.check
    options:
      heading_level: 3

::: tesseradb._database.Database.commit
    options:
      heading_level: 3

::: tesseradb._inserts.Insert
    options:
      show_bases: false

::: tesseradb._reports.Report
    options:
      show_bases: false

::: tesseradb._reports.CommitReport

::: tesseradb._reports.PagedReport
    options:
      show_bases: false
