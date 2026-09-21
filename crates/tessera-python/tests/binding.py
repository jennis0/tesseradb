"""`_tessera`'s behaviour, through the module a Python process imports.

Run by `crates/tessera-python/check.sh`, which builds the extension and puts it on the path.

Every declaration here names no Parquet file, or names one that is not there: a block that names
no file is declared and empty for a check, so the whole surface — a clean check, a refused one,
the payloads, and each way a declaration can fail to be read at all — is reachable without a
corpus to read.
"""

import json
import tempfile
import unittest
from pathlib import Path

import _tessera

DEPLOYMENT = """
[bundle]
path  = "bundle"
cache = "cache"
wal   = "wal.log"

[plugin]
module = "builtin:passthrough"

[disclosure]
token_max_lifetime = 3600

[serve]
viewer  = "127.0.0.1:37585"
session = "127.0.0.1:49303"
control = "127.0.0.1:45721"
"""

DECLARED_AND_EMPTY = """
[[view]]
name = "s0"
extent = { min = 0.0, max = 10.0 }
point_visibility = { default = "public" }

[[attribute]]
name = "score"
type = "f32"
"""

NAMES_A_FILE_THAT_IS_NOT_THERE = """
[sources]
points = "points.parquet"

[[view]]
name = "s0"
extent = { min = 0.0, max = 10.0 }
source = "points"
point_visibility = { default = "public" }
"""


def project(declaration: str, deployment: str = DEPLOYMENT) -> str:
    """A deployment directory holding `declaration`, and the path of its `tessera.toml`."""
    directory = Path(tempfile.mkdtemp())
    (directory / "tessera.toml").write_text(deployment, encoding="utf-8")
    (directory / "schema.toml").write_text(declaration, encoding="utf-8")
    return str(directory / "tessera.toml")


class ACleanDeclaration(unittest.TestCase):
    def test_checks(self):
        result = _tessera.check(project(DECLARED_AND_EMPTY))
        self.assertTrue(result.ok)
        self.assertEqual(result.findings, [])

    def test_names_every_block_it_looked_at_and_the_file_each_had(self):
        result = _tessera.check(project(DECLARED_AND_EMPTY))
        looked_at = {(source.object.block, source.object.name) for source in result.sources}
        self.assertIn(("view", "s0"), looked_at)
        self.assertIn(("attribute", "score"), looked_at)
        self.assertEqual([source.path for source in result.sources], [None] * len(result.sources))

    def test_serialises_to_payloads_addressed_by_block_kind(self):
        payloads = json.loads(_tessera.payloads(project(DECLARED_AND_EMPTY)))
        self.assertEqual([view["name"] for view in payloads["views"]], ["s0"])
        self.assertEqual([body["name"] for body in payloads["attributes"]], ["score"])

    def test_payloads_are_text_a_second_call_repeats(self):
        deployment = project(DECLARED_AND_EMPTY)
        self.assertEqual(_tessera.payloads(deployment), _tessera.payloads(deployment))


class ADeclarationThatDoesNotCheck(unittest.TestCase):
    def test_comes_back_as_a_result_rather_than_an_exception(self):
        result = _tessera.check(project(NAMES_A_FILE_THAT_IS_NOT_THERE))
        self.assertFalse(result.ok)
        self.assertTrue(result.findings)

    def test_each_finding_names_the_block_it_is_about(self):
        result = _tessera.check(project(NAMES_A_FILE_THAT_IS_NOT_THERE))
        for finding in result.findings:
            self.assertIn(finding.object.block, ("view", "source", "attribute", "layer"))
            self.assertTrue(finding.object.name)
            self.assertTrue(finding.detail)

    def test_emits_no_payloads(self):
        with self.assertRaises(_tessera.DeclarationError) as refusal:
            _tessera.payloads(project(NAMES_A_FILE_THAT_IS_NOT_THERE))
        self.assertTrue(refusal.exception.findings)
        self.assertEqual(refusal.exception.findings[0].object.block, "view")
        self.assertEqual(refusal.exception.findings[0].object.name, "s0")


class ADeclarationThatCannotBeReadAtAll(unittest.TestCase):
    def refusal(self, deployment_path: str, call):
        with self.assertRaises(_tessera.DeclarationError) as raised:
            call(deployment_path)
        self.assertTrue(raised.exception.findings)
        return raised.exception.findings[0]

    def test_a_deployment_file_that_is_not_there_is_refused_by_both_calls(self):
        missing = str(Path(tempfile.mkdtemp()) / "tessera.toml")
        for call in (_tessera.check, _tessera.payloads):
            finding = self.refusal(missing, call)
            self.assertEqual(finding.object.block, "deployment")
            self.assertEqual(finding.object.name, missing)

    def test_a_deployment_file_that_does_not_parse_is_refused(self):
        finding = self.refusal(project(DECLARED_AND_EMPTY, "[bundle\n"), _tessera.check)
        self.assertEqual(finding.object.block, "deployment")

    def test_a_declaration_that_does_not_parse_is_refused_and_names_the_file(self):
        finding = self.refusal(project("[[view]]\nname =\n"), _tessera.check)
        self.assertEqual(finding.object.block, "declaration")
        self.assertTrue(finding.object.name.endswith("schema.toml"))


class TheFindingsAreReadable(unittest.TestCase):
    def test_a_finding_prints_the_block_it_is_about_and_why(self):
        result = _tessera.check(project(NAMES_A_FILE_THAT_IS_NOT_THERE))
        printed = str(result.findings[0])
        self.assertIn("view 's0'", printed)
        self.assertIn(result.findings[0].detail, printed)

    def test_a_refusal_prints_what_it_carries(self):
        with self.assertRaises(_tessera.DeclarationError) as refusal:
            _tessera.payloads(project(NAMES_A_FILE_THAT_IS_NOT_THERE))
        self.assertIn(str(refusal.exception.findings[0]), str(refusal.exception))


if __name__ == "__main__":
    unittest.main()
