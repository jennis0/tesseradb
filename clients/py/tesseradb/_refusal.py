"""The one exception the package raises for a request it or the server will not carry out."""

from __future__ import annotations


class Refusal(ValueError):
    """A request that was refused, with a message saying what was wrong and what to do instead.

    The package raises it for a call it will not make, and for a request the server refused, in
    which case the message carries the server's status and answer. A `commit()` that did nothing
    carries its report as `report`, which is `None` otherwise.

        try:
            db.commit()
        except tesseradb.Refusal as refused:
            print(refused.report)
    """

    def __init__(self, message: str = "", report=None) -> None:
        super().__init__(message)
        self.report = report
