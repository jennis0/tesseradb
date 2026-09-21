"""The session plane's ``authorise``, and the token it returns.

Client-components §7: **the entry point is a token.** ``Map(url, token=...)`` is the primary form
— an analyst holds a per-principal token their deployment issued them, as any application's user
does. This module's ``authorise`` is the credential-holding form and is operator-only: the session
credential can mint *any* principal, and a notebook that takes it is client-interaction §7's
pooled-service-token anti-pattern in a cell. It exists for the local, single-principal case and
for the demo, and it never lets the credential reach the browser — the token it mints does.
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
    """A viewer token and when it expires (seconds since the epoch, as the server reports it).

    ``renew`` is set when the token came from ``authorise`` and can be minted again; a token handed
    in as a string has no renewal, and a widget holding one reports ``expired`` when the server
    refuses it rather than asking for another. ``terms`` is what it was minted for, where this
    process minted it, and ``None`` for a token handed in as a string: the holder of one cannot
    read what it grants.
    """

    token: str
    expires_at: Optional[float] = None
    #: The non-capability handle ``revoke`` takes, and ``None`` for a token handed in as a string.
    token_id: Optional[int] = None
    renew: Optional[Callable[[], "Token"]] = field(default=None, repr=False, compare=False)
    terms: Optional[Sequence[str]] = field(default=None, repr=False, compare=False)

    @property
    def seconds_left(self) -> Optional[float]:
        return None if self.expires_at is None else self.expires_at - time.time()

    def __repr__(self) -> str:
        """The expiry and how many terms, and never the token itself.

        A repr is printed by a cell that returns one, by a traceback and by a logger, and a token
        printed in a notebook is a token in the saved file.
        """
        left = "" if self.seconds_left is None else f", {self.seconds_left:.0f}s left"
        granting = "" if self.terms is None else f", {len(self.terms)} term(s)"
        return f"Token(expires_at={self.expires_at}{left}{granting})"


#: What a token may be given as: the token itself, a `Token`, or a callable returning either. A
#: callable is what an issuer with its own renewal looks like from here.
TokenSource = Union[str, Token, Callable[[], Union[str, "Token"]]]


def minted(source: TokenSource) -> Token:
    """One token from a token source, whatever shape the source is.

    The widget and the query verbs both take a source and both have to make a token of it, and a
    second reading of what a source may be is a second set of shapes one of them accepts and the
    other does not.
    """
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
    """**Operator-only.** Mint a viewer token for the principal whose visibility is ``terms``.

    The session credential this takes can mint a token for *any* principal, so whoever holds it
    holds every principal's view. That makes this the wrong entry point for an analyst's notebook:
    the shape a practitioner writes when the SDK offers nothing else is one credential in one cell
    filtering per user afterwards — the pooled service token of client-interaction §7, under which
    every count and density a user sees derives from the credential's mask, not theirs. Hand an
    analyst a token instead (``Map(url, token=...)``); use this for the local single-principal
    case and for the demo, where the operator and the analyst are one person.

    ``terms`` is the principal's term list, passed to the server's passthrough auth plugin as bare
    claims (``{"terms": [...]}``, base64 in ``auth_data``). The returned ``Token`` renews itself
    on request — ``token.renew()`` — with the same credential, which stays in this process.

    Refuses without a credential: an empty one would be sent and refused by the server, but the
    refusal there reads as a bad deployment rather than a missing argument.
    """
    if not credential:
        raise ValueError("authorise needs the session credential (operator-only; see the docstring)")
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
    """**Operator-only.** End a session by the ``token_id`` its minting returned.

    It takes the ``token_id`` and never the token, so the capability itself never transits a
    second time; a ``Token`` this process minted may be passed instead, and its handle is read off
    it. A handle naming no live session is accepted in silence, there being nothing to say about
    it that would not enumerate the sessions that are live.

    This is the session plane and takes the session credential, as ``authorise`` does. A viewer
    holding only a token cannot revoke itself.
    """
    if not credential:
        raise ValueError("revoke needs the session credential (operator-only; see the docstring)")
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
