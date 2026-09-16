"""What the SDK refuses, in the design's own terms.

One exception type, so a notebook can catch the SDK's refusals apart from an error raised by
pyarrow or by the standard library. A refusal says what was wrong and what to write instead;
anything the binary decides is left to the binary, whose output the report carries.
"""

from __future__ import annotations


class Refusal(ValueError):
    """The SDK will not do this, and the message says what to do instead."""
