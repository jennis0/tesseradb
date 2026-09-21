import json
import base64
import io
import urllib.error
import urllib.request

import pytest

from tesseradb import Token, authorise


def test_authorise_refuses_without_a_credential():
    with pytest.raises(ValueError, match="operator-only"):
        authorise("http://127.0.0.1:1", "", ["a"])
    with pytest.raises(ValueError):
        authorise("", "cred", ["a"])


def test_authorise_posts_bare_claims_and_returns_a_renewable_token(monkeypatch):
    seen = []

    class Response(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *a):
            return False

    def fake_urlopen(request, timeout):
        seen.append(request)
        return Response(json.dumps({"token": f"tok-{len(seen)}", "token_id": 1, "expires_at": 1800000000}).encode())

    monkeypatch.setattr(urllib.request, "urlopen", fake_urlopen)
    token = authorise("http://session.test/", "cred", ["x", "y"])
    assert token == Token("tok-1", 1800000000.0, token_id=1)
    req = seen[0]
    assert req.full_url == "http://session.test/session/authorise"
    assert req.get_header("Authorization") == "Bearer cred"
    body = json.loads(req.data)
    assert json.loads(base64.b64decode(body["auth_data"])) == {"terms": ["x", "y"]}
    renewed = token.renew()
    assert renewed.token == "tok-2" and renewed.renew is not None


def test_authorise_surfaces_the_servers_refusal(monkeypatch):
    def refuse(request, timeout):
        raise urllib.error.HTTPError(request.full_url, 401, "unauthorised", {}, io.BytesIO(b'{"code":"bad-credential"}'))

    monkeypatch.setattr(urllib.request, "urlopen", refuse)
    with pytest.raises(PermissionError, match="401"):
        authorise("http://session.test", "wrong", ["x"])
