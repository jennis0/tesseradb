"""Tokens: what a reader presents to read, and how an operator makes and ends them.

A reader normally holds a token its deployment issued. `authorise` makes one from the session
credential, which can make a token for any set of access terms, so only an operator should hold
it. The credential stays in this process; only the token reaches a browser.
"""

from __future__ import annotations

import base64
import json
import time
import urllib.error
import urllib.request
from dataclasses import dataclass, field
from typing import Callable, Optional, Sequence, Union

from ._refusal import Refusal


@dataclass
class Token:
    """A token for reading a Tessera database, and when it expires.

    - `token`: the token itself, a string to keep secret.
    - `expires_at`: when it expires, in seconds since 1970, or `None` if not known.
    - `token_id`: a handle that `revoke` takes to end it without sending the token again. It is
      `None` for a token given as a string.
    - `renew`: a function that makes a fresh token, set when `authorise` made this one.
    - `terms`: the access terms it grants, where `authorise` made it, and `None` otherwise.

    Printing a token shows its expiry and how many terms it grants, never the token.
    """

    token: str
    expires_at: Optional[float] = None
    token_id: Optional[int] = None
    renew: Optional[Callable[[], "Token"]] = field(default=None, repr=False, compare=False)
    terms: Optional[Sequence[str]] = field(default=None, repr=False, compare=False)

    @property
    def seconds_left(self) -> Optional[float]:
        """Seconds until the token expires, or `None` if its expiry is not known."""
        return None if self.expires_at is None else self.expires_at - time.time()

    def __repr__(self) -> str:
        # Never the token: a printed token ends up in saved notebooks and logs.
        left = "" if self.seconds_left is None else f", {self.seconds_left:.0f}s left"
        granting = "" if self.terms is None else f", {len(self.terms)} term(s)"
        return f"Token(expires_at={self.expires_at}{left}{granting})"


#: What a token may be given as: the token itself, a `Token`, or a function returning either.
TokenSource = Union[str, Token, Callable[[], Union[str, "Token"]]]


def minted(source: TokenSource) -> Token:
    """A `Token` from any of the forms a token may be given in."""
    got = source() if callable(source) and not isinstance(source, Token) else source
    if isinstance(got, str):
        got = Token(got)
    if not isinstance(got, Token) or not got.token:
        raise Refusal(
            f"a token must be a string, a Token or a callable returning one; got {got!r}"
        )
    return got


def authorise(
    session_url: str,
    credential: str,
    terms: Sequence[str],
    *,
    timeout: float = 10.0,
) -> Token:
    """Make a token that reads as someone holding the access terms given. For operators.

    - `session_url`: the address of the database's session endpoint.
    - `credential`: the session credential. It can make a token for any terms, so whoever holds
      it can read everything. Give other people a token, never the credential.
    - `terms`: the access terms the token grants.
    - `timeout`: how long to wait for the server, in seconds.

    The token's `renew()` makes a fresh one with the same credential, which stays in this
    process. A `Database` does this for you: `db.token(terms)` and `db.viewer(terms)`.

        token = tesseradb.authorise(session_url, credential, ["cs.LG"])
        tesseradb.connect(viewer_url, token).view("papers").count()
    """
    if not credential:
        raise ValueError("authorise needs the session credential")
    if not session_url:
        raise ValueError("authorise needs the session plane's URL")
    auth_data = base64.b64encode(json.dumps({"terms": list(terms)}).encode()).decode()
    body = json.dumps({"auth_data": auth_data}).encode()
    request = urllib.request.Request(
        session_url.rstrip("/") + "/session/authorise",
        data=body,
        method="POST",
        headers={
            "authorization": f"Bearer {credential}",
            "content-type": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            answer = json.loads(response.read())
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        raise PermissionError(f"/session/authorise refused ({e.code}): {detail}") from None
    return Token(
        token=answer["token"],
        expires_at=float(answer["expires_at"]),
        token_id=int(answer["token_id"]),
        renew=lambda: authorise(session_url, credential, terms, timeout=timeout),
        terms=list(terms),
    )


def revoke(
    session_url: str,
    credential: str,
    token_id: Union[int, Token],
    *,
    timeout: float = 10.0,
) -> None:
    """End a token so it can no longer read. For operators.

    - `session_url`, `credential`: as for `authorise`.
    - `token_id`: the token's `token_id`, or the `Token` itself. Only the id is sent.
    - `timeout`: how long to wait for the server, in seconds.

    An id that names no live token is accepted without comment, so the answer says nothing
    about which tokens exist.

        tesseradb.revoke(session_url, credential, token)
    """
    if not credential:
        raise ValueError("revoke needs the session credential")
    if not session_url:
        raise ValueError("revoke needs the session plane's URL")
    handle = token_id.token_id if isinstance(token_id, Token) else token_id
    if handle is None:
        raise ValueError(
            "revoke needs a token_id: this Token was handed in as a string and carries none. "
            "Pass the token_id /session/authorise returned"
        )
    request = urllib.request.Request(
        session_url.rstrip("/") + "/session/revoke",
        data=json.dumps({"token_id": int(handle)}).encode(),
        method="POST",
        headers={
            "authorization": f"Bearer {credential}",
            "content-type": "application/json",
        },
    )
    try:
        urllib.request.urlopen(request, timeout=timeout).close()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        raise PermissionError(f"/session/revoke refused ({e.code}): {detail}") from None
