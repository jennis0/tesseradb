"""Tokens: what a reader presents to read, and how one is made and ended.

A reader holds a token its deployment issued, or makes one with `login` from its own credential:
a password, an API key or an OIDC access token. `authorise` makes one on the session plane: for
another principal, with an API key whose principal holds `authorise-as`, as an integrator's backend
does, or for a set of terms, with the operator credential. Either credential can make a token that
reads as anyone, so it stays where it is kept. Only the token reaches a browser.
"""

from __future__ import annotations

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
    - `expires_at`: when its session ends, in seconds since 1970, or `None` if not known. A
      catalogue change, such as a grant to its principal, can end it sooner.
    - `token_id`: a handle that `revoke` takes to end it without sending the token again. It is
      set where `authorise` made the token, and `None` otherwise.
    - `renew`: a function that makes a fresh token with the same credential, set where `login`
      or `authorise` made this one.
    - `principal`: the local principal it reads as, where `authorise` made it for one or `login`
      was given a password, and `None` otherwise.
    - `terms`: the access terms it holds, where `authorise` made it for them, and `None`
      otherwise.

    Printing a token shows its expiry, its principal and how many terms it holds, never the token.
    """

    token: str
    expires_at: Optional[float] = None
    token_id: Optional[int] = None
    renew: Optional[Callable[[], "Token"]] = field(default=None, repr=False, compare=False)
    principal: Optional[str] = field(default=None, compare=False)
    terms: Optional[Sequence[str]] = field(default=None, repr=False, compare=False)

    @property
    def seconds_left(self) -> Optional[float]:
        """Seconds until the token expires, or `None` if its expiry is not known."""
        return None if self.expires_at is None else self.expires_at - time.time()

    def __repr__(self) -> str:
        # Never the token: a printed token ends up in saved notebooks and logs.
        left = "" if self.seconds_left is None else f", {self.seconds_left:.0f}s left"
        who = "" if self.principal is None else f", principal={self.principal!r}"
        holding = "" if self.terms is None else f", {len(self.terms)} term(s)"
        return f"Token(expires_at={self.expires_at}{left}{who}{holding})"


TokenSource = Union[str, Token, Callable[[], Union[str, "Token"]]]
"""What a token may be given as: the token itself as a string, a `Token`, or a function that
returns either, which is called again when the token it gave is close to expiry."""


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


def _post(url: str, bearer: Optional[str], body: Optional[dict], timeout: float, verb: str):
    """One POST, answering the parsed JSON body, or `None` where there is none."""
    headers = {} if bearer is None else {"authorization": f"Bearer {bearer}"}
    data = None
    if body is not None:
        headers["content-type"] = "application/json"
        data = json.dumps(body).encode()
    request = urllib.request.Request(url, data=data, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            text = response.read()
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        raise PermissionError(f"{verb} refused ({e.code}): {detail}") from None
    return json.loads(text) if text else None


def login(
    viewer_url: str,
    *,
    principal: Optional[str] = None,
    password: Optional[str] = None,
    api_key: Optional[str] = None,
    access_token: Optional[str] = None,
    timeout: float = 10.0,
) -> Token:
    """Make a token that reads as the principal a credential authenticates.

    - `viewer_url`: the address of the database's viewer plane, where readers read.
    - Exactly one credential: `principal` and `password` together, `api_key`, or
      `access_token`, an OIDC access token from a provider the database accepts.
    - `timeout`: how long to wait for the server, in seconds.

    The principal must hold `read`. The token's `renew()` logs in again with the same
    credential, which stays in this process.

    A missing `viewer_url`, or anything but exactly one credential, raises `ValueError`. A
    credential the server does not accept raises `PermissionError`; the answer is the same
    whatever the reason, so it does not say whether a principal exists.

        token = tesseradb.login(viewer_url, principal="ann", password=password)
        tesseradb.connect(viewer_url, token).view("papers").count()
    """
    if not viewer_url:
        raise ValueError("login needs the viewer plane's URL")
    given = [
        principal is not None or password is not None,
        api_key is not None,
        access_token is not None,
    ]
    if sum(given) != 1 or (given[0] and (principal is None or password is None)):
        raise ValueError(
            "login takes exactly one credential: principal= with password=, api_key=, or "
            "access_token="
        )
    if given[0]:
        body: dict = {"password": {"principal": principal, "password": password}}
    elif api_key is not None:
        body = {"api_key": api_key}
    else:
        body = {"access_token": access_token}
    answer = _post(viewer_url.rstrip("/") + "/v1/login", None, body, timeout, "/v1/login")
    return Token(
        token=answer["token"],
        expires_at=float(answer["expires_at"]),
        renew=lambda: login(
            viewer_url,
            principal=principal,
            password=password,
            api_key=api_key,
            access_token=access_token,
            timeout=timeout,
        ),
        principal=principal,
    )


def logout(viewer_url: str, token: Union[str, Token], *, timeout: float = 10.0) -> None:
    """End the session of a token, so it can no longer read.

    - `viewer_url`: the address of the database's viewer plane.
    - `token`: the token, as a string or a `Token`.

    A token whose session has already ended raises `PermissionError`.
    """
    if not viewer_url:
        raise ValueError("logout needs the viewer plane's URL")
    bearer = token.token if isinstance(token, Token) else token
    _post(viewer_url.rstrip("/") + "/v1/logout", bearer, None, timeout, "/v1/logout")


def authorise(
    session_url: str,
    credential: str,
    *,
    principal: Optional[str] = None,
    access_token: Optional[str] = None,
    terms: Optional[Sequence[str]] = None,
    timeout: float = 10.0,
) -> Token:
    """Make a token on the session plane, for another principal or for a set of terms.

    - `session_url`: the address of the database's session plane, where tokens are made.
    - `credential`: an API key whose principal holds `authorise-as`, or the operator credential.
      Either can make a token that reads as anyone. Give other people a token, never the
      credential.
    - Exactly one target: `principal`, a local principal's name; `access_token`, the OIDC access
      token of the identity to act as; or `terms`, the access terms the token holds, which only
      the operator credential may name.
    - `timeout`: how long to wait for the server, in seconds.

    A token for a principal carries its terms and its `read` and `write`, and the principal must
    hold `read`. A token for `terms` holds those terms and `read`. The token's `renew()` makes a
    fresh one with the same credential, which stays in this process. A `Database` does this for
    you: `db.token(terms)` and `db.viewer(terms)`.

    An empty `session_url` or `credential`, or anything but exactly one target, raises
    `ValueError`, and a request the server refuses raises `PermissionError` with its status and
    answer.

        token = tesseradb.authorise(session_url, api_key, principal="ann")
        token = tesseradb.authorise(session_url, operator_credential, terms=["cs.LG"])
        tesseradb.connect(viewer_url, token).view("papers").count()
    """
    if not credential:
        raise ValueError(
            "authorise needs a credential: an API key whose principal holds authorise-as, or the "
            "operator credential"
        )
    if not session_url:
        raise ValueError("authorise needs the session plane's URL")
    named = {
        "principal": principal,
        "access_token": access_token,
        "terms": None if terms is None else list(terms),
    }
    body = {k: v for k, v in named.items() if v is not None}
    if len(body) != 1:
        raise ValueError("authorise takes exactly one of principal=, access_token= and terms=")
    answer = _post(
        session_url.rstrip("/") + "/session/authorise",
        credential,
        body,
        timeout,
        "/session/authorise",
    )
    return Token(
        token=answer["token"],
        expires_at=float(answer["expires_at"]),
        token_id=int(answer["token_id"]),
        renew=lambda: authorise(
            session_url,
            credential,
            principal=principal,
            access_token=access_token,
            terms=terms,
            timeout=timeout,
        ),
        principal=principal,
        terms=named["terms"],
    )


def revoke(
    session_url: str,
    credential: str,
    token_id: Union[int, Token],
    *,
    timeout: float = 10.0,
) -> None:
    """End a token `authorise` made, so it can no longer read.

    - `session_url`, `credential`: as for `authorise`. An API key ends only a token minted with a
      key of the same principal, and the operator credential ends any.
    - `token_id`: the token's `token_id`, or the `Token` itself. Only the id is sent.
    - `timeout`: how long to wait for the server, in seconds.

    An id that names no such live token is accepted without comment, so the answer says nothing
    about which tokens exist. An empty `session_url` or `credential`, or a `Token` with no
    `token_id`, raises `ValueError`, and a request the server refuses raises `PermissionError`.

        tesseradb.revoke(session_url, credential, token)
    """
    if not credential:
        raise ValueError(
            "revoke needs a credential: an API key whose principal holds authorise-as, or the "
            "operator credential"
        )
    if not session_url:
        raise ValueError("revoke needs the session plane's URL")
    handle = token_id.token_id if isinstance(token_id, Token) else token_id
    if handle is None:
        raise ValueError(
            "revoke needs a token_id: this Token was not made by authorise and carries none. "
            "Pass the token_id /session/authorise returned, or end it with logout"
        )
    _post(
        session_url.rstrip("/") + "/session/revoke",
        credential,
        {"token_id": int(handle)},
        timeout,
        "/session/revoke",
    )
