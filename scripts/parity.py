"""Compare the Rust engine's reports with the Python reference on the same bundles.

    python scripts/parity.py data/live [more bundle dirs or bundle .json files] [--opts '{"start": ...}']

For every bundle both engines build the full and the shareable report; the script compares every chart
spec (numbers within a tolerance) and the visible page text, and prints the differences it finds.
Exit status 1 if anything differs beyond tolerance.
"""
from __future__ import annotations

import argparse
import difflib
import json
import math
import re
import subprocess
import sys
import tempfile
from html.parser import HTMLParser
from pathlib import Path

root = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(root))

import pandas as pd  # noqa: E402

from alpaca_analyzer.bundle import Bundle  # noqa: E402
from alpaca_analyzer.web import build_reports  # noqa: E402

# pandas' default sort (numpy quicksort) puts equal keys in an arbitrary order; the Rust engine keeps
# them in their original order. Make the reference stable too, so only real differences show up.
for _cls in (pd.DataFrame, pd.Series):
    _orig = _cls.sort_values

    def _stable(self, *a, _orig=_orig, **kw):
        kw.setdefault("kind", "stable")
        return _orig(self, *a, **kw)
    _cls.sort_values = _stable

RUST = root / "engine" / "target" / "release" / "aa-report"
# Monte Carlo fill odds use different random streams: compare loosely
LOOSE = re.compile(r"^(<\d+d|Before expiry)$")


def bundle_json(src: Path) -> str:
    if src.is_dir():
        b = Bundle.load(src)
        return json.dumps({k: getattr(b, k) for k in ("activities", "portfolio_history", "positions", "account", "bars",
                                                       "prices", "option_snapshots", "orders", "stock_snapshots",
                                                       "clock")})
    return src.read_text()


def figs_of(html: str) -> dict:
    m = re.search(r"const FIGS = (.*?);\n", html, re.S)
    return json.loads(m.group(1).replace("<\\/", "</")) if m else {}


def rp_of(html: str):
    m = re.search(r'<script type="application/json" id="rp-data">(.*?)</script>', html, re.S)
    return json.loads(m.group(1).replace("<\\/", "</")) if m else None


class Text(HTMLParser):
    """Visible text and tags (without data-v). Cells of the chance-of-filling table become '%' (their
    Monte Carlo values are compared separately); re-pricing rows are sorted (ties are unordered)."""

    def __init__(self):
        super().__init__()
        self.out = []
        self.skip = 0
        self.fill: list[float] = []
        self.in_fill = self.in_rp = False
        self.row: list[str] | None = None
        self.rp_rows: list[list[str]] = []

    def emit(self, tok):
        if self.in_rp and self.row is not None:
            self.row.append(tok)
        else:
            self.out.append(tok)

    def handle_starttag(self, tag, attrs):
        d = dict(attrs)
        if tag == "script":
            self.skip += 1
        if tag == "table" and d.get("id") == "t_fill":
            self.in_fill = True
        if tag == "table" and "rp-table" in (d.get("class") or ""):
            self.in_rp = True
        if self.in_rp and tag == "tr" and d.get("data-und"):
            self.row = []
            self.rp_rows.append(self.row)
        a = " ".join(f'{k}="{v}"' for k, v in attrs if k not in ("data-v",))
        self.emit(f"<{tag}{' ' + a if a else ''}>")

    def handle_endtag(self, tag):
        if tag == "script":
            self.skip -= 1
        if tag == "table" and self.in_fill:
            self.in_fill = False
        if tag == "table" and self.in_rp:
            self.in_rp = False
            self.row = None
            for r in sorted(self.rp_rows):
                self.out.extend(r)
            self.rp_rows = []

    def handle_data(self, data):
        if self.skip or not data.strip():
            return
        t = data.strip()
        if self.in_fill and re.fullmatch(r"[<>]?\d+%", t):
            self.fill.append(float(t.strip("<>%")))
            t = "%"
        self.emit(t)


def body_tokens(html: str) -> tuple[list[str], list[float]]:
    m = re.search(r"<main>(.*)</main>", html, re.S)
    p = Text()
    p.feed(m.group(1) if m else html)
    return p.out, p.fill


def close(a, b, rel=1e-6, abs_=1e-6) -> bool:
    if isinstance(a, bool) or isinstance(b, bool):
        return a == b
    if isinstance(a, (int, float)) and isinstance(b, (int, float)):
        if math.isnan(a) and math.isnan(b):
            return True
        return abs(a - b) <= max(abs_, rel * max(abs(a), abs(b)))
    return a == b


def diff_json(a, b, path, out, loose=False):
    if len(out) > 40:
        return
    if isinstance(a, dict) and isinstance(b, dict):
        for k in sorted(set(a) | set(b)):
            if k not in a or k not in b:
                out.append(f"{path}.{k}: only in {'python' if k in a else 'rust'}")
            else:
                diff_json(a[k], b[k], f"{path}.{k}", out, loose)
    elif isinstance(a, list) and isinstance(b, list):
        if len(a) != len(b):
            out.append(f"{path}: length {len(a)} vs {len(b)}")
            return
        for i, (x, y) in enumerate(zip(a, b)):
            diff_json(x, y, f"{path}[{i}]", out, loose)
    elif not close(a, b, rel=0.05 if loose else 1e-6, abs_=0.05 if loose else 1e-6):
        out.append(f"{path}: {a!r} vs {b!r}")


NUM = re.compile(r"[−+-]?\$?\d[\d,]*(?:\.\d+)?%?")


def tokens_equal(a: str, b: str) -> bool:
    if a == b:
        return True
    # tolerate last-digit rounding differences in numbers embedded in text
    na, nb = NUM.findall(a), NUM.findall(b)
    if len(na) != len(nb) or NUM.sub("#", a) != NUM.sub("#", b):
        return False
    for x, y in zip(na, nb):
        fx = float(re.sub(r"[^\d.]", "", x) or 0)
        fy = float(re.sub(r"[^\d.]", "", y) or 0)
        if x.lstrip("$")[:1] in "−-" != (y.lstrip("$")[:1] in "−-") or abs(fx - fy) > max(0.011, 1e-6 * max(fx, fy)):
            return False
    return True


def compare(name: str, py: dict, rs: dict) -> int:
    bad = 0
    for view in ("full", "share"):
        fp, fr = figs_of(py[view]), figs_of(rs[view])
        if set(fp) != set(fr):
            print(f"[{name}/{view}] charts only in python: {sorted(set(fp) - set(fr))}; only in rust: {sorted(set(fr) - set(fp))}")
            bad += 1
        for fid in sorted(set(fp) & set(fr)):
            out: list[str] = []
            diff_json(fp[fid], fr[fid], fid, out)
            if out:
                bad += 1
                print(f"[{name}/{view}] chart {fid}: {len(out)} differences")
                for line in out[:8]:
                    print("    ", line[:300])
        rp_p, rp_r = rp_of(py[view]), rp_of(rs[view])
        out = []
        diff_json(rp_p, rp_r, "rp-data", out)
        if out:
            bad += 1
            print(f"[{name}/{view}] re-pricing payload: {len(out)} differences")
            for line in out[:8]:
                print("    ", line[:300])
        (tp, fill_p), (tr, fill_r) = body_tokens(py[view]), body_tokens(rs[view])
        if len(fill_p) != len(fill_r):
            bad += 1
            print(f"[{name}/{view}] chance-of-filling table: {len(fill_p)} vs {len(fill_r)} cells")
        elif fill_p:
            worst = max(abs(x - y) for x, y in zip(fill_p, fill_r))
            print(f"[{name}/{view}] chance of filling (Monte Carlo): largest gap {worst:.0f} points over {len(fill_p)} cells")
            if worst > 5:
                bad += 1
        sm = difflib.SequenceMatcher(a=tp, b=tr, autojunk=False)
        shown = 0
        for op, i1, i2, j1, j2 in sm.get_opcodes():
            if op == "equal":
                continue
            if op == "replace" and i2 - i1 == j2 - j1 and all(tokens_equal(x, y) for x, y in zip(tp[i1:i2], tr[j1:j2])):
                continue
            ctx = " | ".join(tp[max(0, i1 - 3):i1])[-200:]
            if shown < 25:
                print(f"[{name}/{view}] text {op} after …{ctx}")
                print("     python:", " | ".join(tp[i1:i2])[:400])
                print("     rust:  ", " | ".join(tr[j1:j2])[:400])
            shown += 1
        if shown:
            bad += 1
            print(f"[{name}/{view}] {shown} text differences")
    for k in ("summary", "warnings", "alerts"):
        out = []
        diff_json(py.get(k), rs.get(k), k, out)
        if out:
            bad += 1
            print(f"[{name}] {k}: {out[:5]}")
    return bad


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bundles", nargs="+")
    ap.add_argument("--opts", default="{}")
    args = ap.parse_args()
    total = 0
    for src in args.bundles:
        src = Path(src)
        bj = bundle_json(src)
        side = src.with_suffix(".opts")       # per-bundle options next to a bundle .json
        opts = side.read_text() if src.suffix == ".json" and side.exists() else args.opts
        try:
            py = json.loads(build_reports(bj, opts))
        except Exception as e:  # noqa: BLE001
            py = {"error": str(e)}
        with tempfile.TemporaryDirectory() as td:
            (Path(td) / "b.json").write_text(bj)
            (Path(td) / "o.json").write_text(opts)
            r = subprocess.run([str(RUST), str(Path(td) / "b.json"), str(Path(td) / "o.json")], capture_output=True, text=True)
        if r.returncode or "error" in py:
            same = r.returncode and "error" in py
            print(f"== {src}: {'both refuse' if same else 'MISMATCH'}: python {py.get('error', 'ok')!r}; rust {r.stderr.strip()!r}")
            total += 0 if same else 1
            continue
        rs = json.loads(r.stdout)
        n = compare(src.name, py, rs)
        print(f"== {src}: {'OK' if not n else f'{n} areas differ'} ({r.stderr.strip()})")
        total += n
    sys.exit(1 if total else 0)


if __name__ == "__main__":
    main()
