# Connecting and creating

`connect` reads a database someone else runs, with a token its operator issued. `create` and `open` give a `Database`: a database in a directory, which runs its own server once it is committed. A token says which access terms its holder reads with.

::: tesseradb._viewer.connect

::: tesseradb._database.create

::: tesseradb._database.open

::: tesseradb._database.Database
    options:
      members: false

::: tesseradb._database.Database.binary
    options:
      heading_level: 3

::: tesseradb._database.Database.save
    options:
      heading_level: 3

::: tesseradb._database.Database.close
    options:
      heading_level: 3

::: tesseradb._database.Database.token
    options:
      heading_level: 3

::: tesseradb._database.Database.revoke
    options:
      heading_level: 3

::: tesseradb._database.Database.viewer_url
    options:
      heading_level: 3

::: tesseradb._database.Database.session_url
    options:
      heading_level: 3

::: tesseradb._database.Database.session_credential
    options:
      heading_level: 3

::: tesseradb._auth.Token

::: tesseradb._auth.authorise

::: tesseradb._auth.revoke

::: tesseradb._refusal.Refusal
