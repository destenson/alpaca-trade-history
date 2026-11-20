"""OCC option symbol parsing (e.g. ``NVDA261120P00152500``)."""
from __future__ import annotations

import re
from dataclasses import dataclass
from datetime import date
from functools import lru_cache

_OCC = re.compile(r"^(?P<root>[A-Z0-9.]{1,6}?)(?P<yy>\d{2})(?P<mm>\d{2})(?P<dd>\d{2})(?P<cp>[CP])(?P<strike>\d{8})$")


@dataclass(frozen=True)
class OptionContract:
    symbol: str
    underlying: str
    expiry: date
    right: str  # "C" or "P"
    strike: float

    @property
    def is_call(self) -> bool:
        return self.right == "C"


@lru_cache(maxsize=None)
def parse_occ(symbol: str) -> OptionContract | None:
    """Return the parsed contract, or ``None`` if ``symbol`` is not an OCC option symbol."""
    if not symbol or len(symbol) < 16:
        return None
    m = _OCC.match(symbol.replace(" ", ""))
    if not m:
        return None
    try:
        expiry = date(2000 + int(m["yy"]), int(m["mm"]), int(m["dd"]))
    except ValueError:
        return None
    return OptionContract(symbol, m["root"], expiry, m["cp"], int(m["strike"]) / 1000.0)


def underlying_of(symbol: str) -> str:
    c = parse_occ(symbol)
    return c.underlying if c else symbol


def is_option(symbol: str) -> bool:
    return parse_occ(symbol) is not None
