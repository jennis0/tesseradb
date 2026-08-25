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
from typing import Callable, Optional, Sequence


@dataclass
class Token:
    """A viewer token and when it expires (seconds since the epoch, as the server reports it).

    ``renew`` is set when the token came from ``authorise`` and can be minted again; a token handed
    in as a string has no renewal, and a widget holding one reports ``expired`` when the server
    refuses it rather than asking for another.
    """

    token: str
    expires_at: Optional[float] = None
    renew: Optional[Callable[[], "Token"]] = field(default=None, repr=False, compare=False)

    @property
    def seconds_left(self) -> Optional[float]:
        return None if self.expires_at is None else self.expires_at - time.time()


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
        renew=lambda: authorise(session_url, credential, terms, timeout=timeout),
    )
