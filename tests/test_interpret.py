import math
from datetime import date

from alpaca_analyzer.interpret import STOCK, Unit, build_units, name_group, readings

LEGS = [("X261123C00130000", 1, 385), ("X261123C00131000", -1, -335), ("X261125C00136000", -1, -485),
        ("X261125C00137000", 1, 432), ("X261125P00134000", -1, -397), ("X261125P00136000", 1, 487)]
OPENED = [["X261123C00130000", "X261123C00131000"],
          ["X261125C00136000", "X261125C00137000", "X261125P00134000", "X261125P00136000"]]


def run():
    units = [Unit(s, q, c, 0.0) for s, q, c in LEGS]
    return readings(units, 128.0, date(2026, 11, 20), OPENED)


def test_as_opened_first_and_alternatives_present():
    rs, whole = run()
    assert rs[0].as_opened and rs[0].label == "Bull call spread + Bearish double vertical"
    labels = {r.label for r in rs}
    assert "Bull call spread + Bear call spread + Bear put spread" in labels
    assert any("Diagonal spread" in l for l in labels)
    assert len(labels) == len(rs)                      # no duplicate readings


def test_same_legs_same_total_but_different_grouped_risk():
    rs, whole = run()
    for r in rs:
        assert math.isclose(sum(g.unrealized for g in r.groups), whole["unrealized"], abs_tol=1e-6)
    opened = rs[0]
    assert opened.undefined == 0 and opened.sum_max_loss == -187.0
    assert any(r.undefined > 0 for r in rs)            # e.g. a naked call appears in some readings
    naked = next(r for r in rs if r.undefined)
    assert any(math.isinf(g.max_loss) for g in naked.groups)


def test_stock_lots_pair_with_options():
    assert name_group([Unit(STOCK, 100, 5000, 5000), Unit("X261218C00055000", -1, -150, -150)]) == "Covered call"
    assert name_group([Unit(STOCK, 100, 5000, 5000), Unit("X261218P00045000", 1, 90, 90)]) == "Protective put"
    units, odd, split = build_units([{"symbol": "X261218C00055000", "qty": -2.0, "cost_basis": -300.0,
                                      "market_value": -250.0}], 250, 12500, 12000, {})
    assert split and sum(u.symbol == STOCK for u in units) == 2 and odd.qty == 50
    rs, _ = readings(units, 50.0, date(2026, 11, 20), None, odd)
    assert any(r.label == "Covered call + Covered call" for r in rs)
