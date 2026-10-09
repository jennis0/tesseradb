# Inserting and committing

`insert` hands a table to something declared and names the columns it reads. `commit` makes what was inserted part of the database: the first commit builds it and starts its server, and each later one sends what was inserted since. `check` says what the next commit would do, and sends nothing.

## Database

::: mosaica._database.Database.insert
    options:
      heading_level: 3

::: mosaica._database.Database.check
    options:
      heading_level: 3

::: mosaica._database.Database.commit
    options:
      heading_level: 3

::: mosaica._inserts.Insert
    options:
      show_bases: false

::: mosaica._reports.Report
    options:
      show_bases: false

::: mosaica._reports.CommitReport

::: mosaica._reports.PagedReport
    options:
      show_bases: false
