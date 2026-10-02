import io
import json
import urllib.error
import urllib.request

import pytest

from tesseradb import Token, authorise, login, logout, revoke


class Response(io.BytesIO):
    def __enter__(self):
        return self

    def __exit__(self, *a):
        return False


def answering(monkeypatch, body: dict | None):
    """Every request is recorded and answered with `body`."""
    seen = []

    def fake_urlopen(request, timeout):
        seen.append(request)
        answer = None if body is None else {**body, "token": f"tok-{len(seen)}"}
        return Response(b"" if answer is None else json.dumps(answer).encode())

    monkeypatch.setattr(urllib.request, "urlopen", fake_urlopen)
    return seen


def test_authorise_and_revoke_refuse_without_a_credential_or_with_two_targets():
    with pytest.raises(ValueError):
        authorise("http://127.0.0.1:1", "", principal="ann")
    with pytest.raises(ValueError):
        authorise("", "key", principal="ann")
    with pytest.raises(ValueError):
        authorise("http://127.0.0.1:1", "key")
    with pytest.raises(ValueError):
        authorise("http://127.0.0.1:1", "key", principal="ann", access_token="jwt")
    with pytest.raises(ValueError):
        authorise("http://127.0.0.1:1", "cred", principal="ann", terms=["a"])
    with pytest.raises(ValueError):
        revoke("http://127.0.0.1:1", "", 1)


def test_authorise_names_the_principal_with_the_key_and_returns_a_renewable_token(monkeypatch):
    seen = answering(monkeypatch, {"token_id": 1, "expires_at": 1800000000})
    token = authorise("http://session.test/", "key", principal="ann")
    assert token == Token("tok-1", 1800000000.0, token_id=1)
    assert token.principal == "ann"
    req = seen[0]
    assert req.full_url == "http://session.test/session/authorise"
    assert req.get_header("Authorization") == "Bearer key"
    assert json.loads(req.data) == {"principal": "ann"}
    renewed = token.renew()
    assert renewed.token == "tok-2" and renewed.renew is not None

    authorise("http://session.test", "key", access_token="jwt")
    assert json.loads(seen[-1].data) == {"access_token": "jwt"}


def test_authorise_names_the_terms_with_the_operator_credential(monkeypatch):
    seen = answering(monkeypatch, {"token_id": 2, "expires_at": 1800000000})
    terms = ["Université de Montréal", "cs.LG"]
    token = authorise("http://session.test", "operator", terms=terms)
    assert token.terms == terms and token.principal is None
    assert seen[0].get_header("Authorization") == "Bearer operator"
    assert json.loads(seen[0].data) == {"terms": terms}
    assert "2 term(s)" in repr(token) and "tok-1" not in repr(token)
    token.renew()
    assert json.loads(seen[-1].data) == {"terms": terms}


def test_login_sends_exactly_one_credential_in_the_body_and_no_bearer(monkeypatch):
    seen = answering(monkeypatch, {"expires_at": 1800000000})
    token = login("http://viewer.test/", principal="ann", password="secret")
    assert token.token == "tok-1" and token.principal == "ann" and token.token_id is None
    assert seen[0].full_url == "http://viewer.test/v1/login"
    assert seen[0].get_header("Authorization") is None
    assert json.loads(seen[0].data) == {"password": {"principal": "ann", "password": "secret"}}
    login("http://viewer.test", api_key="tsk_a_b")
    assert json.loads(seen[-1].data) == {"api_key": "tsk_a_b"}
    login("http://viewer.test", access_token="jwt")
    assert json.loads(seen[-1].data) == {"access_token": "jwt"}
    renewed = token.renew()
    assert renewed.token == f"tok-{len(seen)}"
    assert json.loads(seen[-1].data) == {"password": {"principal": "ann", "password": "secret"}}

    for wrong in (
        {},
        {"principal": "ann"},
        {"password": "secret"},
        {"api_key": "k", "access_token": "jwt"},
        {"principal": "ann", "password": "secret", "api_key": "k"},
    ):
        with pytest.raises(ValueError):
            login("http://viewer.test", **wrong)


def test_logout_sends_the_token_as_its_bearer(monkeypatch):
    seen = answering(monkeypatch, None)
    logout("http://viewer.test", Token("tok-9"))
    assert seen[0].full_url == "http://viewer.test/v1/logout"
    assert seen[0].get_header("Authorization") == "Bearer tok-9"
    assert seen[0].data is None


def test_a_refusal_is_a_permission_error(monkeypatch):
    def refuse(request, timeout):
        raise urllib.error.HTTPError(
            request.full_url, 401, "unauthorised", {}, io.BytesIO(b'{"error":"bad-credential"}')
        )

    monkeypatch.setattr(urllib.request, "urlopen", refuse)
    with pytest.raises(PermissionError):
        authorise("http://session.test", "wrong", principal="ann")
    with pytest.raises(PermissionError):
        login("http://viewer.test", api_key="wrong")
