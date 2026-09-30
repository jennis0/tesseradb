"""The harness that runs the Python blocks in the tutorials and guides, run on pages of its own."""


def run(pytester, page: str):
    pytester.makefile(".md", page=page)
    return pytester.runpytest("--markdown-docs", "--markdown-docs-syntax=superfences")


def test_a_passing_block_passes(pytester):
    run(pytester, "```python\nassert 1 + 1 == 2\n```\n").assert_outcomes(passed=1)


def test_a_failing_block_fails(pytester):
    run(pytester, "```python\nassert 1 + 1 == 3\n```\n").assert_outcomes(failed=1)


def test_a_notest_block_is_not_run(pytester):
    page = "``` {.python notest}\nassert 1 + 1 == 3\n```\n\n```python\nassert True\n```\n"
    run(pytester, page).assert_outcomes(passed=1)
