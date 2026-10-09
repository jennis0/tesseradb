# Connecting and creating

`connect` reads a database someone else runs, with a token: one its operator issued, one `login` makes from a password, an API key or an OIDC access token, or one `authorise` makes. `create` and `open` give a `Database`: a database in a directory, which runs its own server once it is committed, and makes its tokens with its own operator credential. A token says which access terms its holder reads with: those granted to the principal it was made for, or those the operator named.

::: mosaica._viewer.connect

::: mosaica._database.create

::: mosaica._database.open

::: mosaica._database.Database
    options:
      members: false

::: mosaica._database.Database.binary
    options:
      heading_level: 3

::: mosaica._database.Database.save
    options:
      heading_level: 3

::: mosaica._database.Database.close
    options:
      heading_level: 3

::: mosaica._database.Database.token
    options:
      heading_level: 3

::: mosaica._database.Database.revoke
    options:
      heading_level: 3

::: mosaica._database.Database.viewer_url
    options:
      heading_level: 3

::: mosaica._database.Database.session_url
    options:
      heading_level: 3

::: mosaica._database.Database.operator_credential
    options:
      heading_level: 3

::: mosaica._auth.Token

::: mosaica._auth.TokenSource

::: mosaica._auth.login

::: mosaica._auth.logout

::: mosaica._auth.authorise

::: mosaica._auth.revoke

::: mosaica._refusal.Refusal
