"""Re-exports `oracle.wire.decode_viewport` (Task 15's refactor moved the implementation into
`reference/oracle/wire.py` so `conformance/tests` could reuse it without copy-paste). Kept as a
thin shim so `reference/tests/test_differential.py`'s `from .wire import decode_viewport` keeps
working unchanged.
"""

from __future__ import annotations

from oracle.wire import decode_viewport, split_frames  # noqa: F401
