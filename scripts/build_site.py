"""Assemble the static GitHub Pages site into ./site (or the directory given as argv[1]).

site/
  index.html, app.js, style.css      <- web/
  alpaca_analyzer-<ver>-py3-none-any.whl   <- this package, installed in the browser by micropip
  manifest.json                      <- {"wheel": "<wheel filename>"}
"""
import json
import shutil
import subprocess
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
out = Path(sys.argv[1] if len(sys.argv) > 1 else root / "site").resolve()
if out.exists():
    shutil.rmtree(out)
shutil.copytree(root / "web", out)
subprocess.run([sys.executable, "-m", "pip", "wheel", str(root), "--no-deps", "-q",
                "-w", str(out)], check=True)
wheels = sorted(out.glob("alpaca_analyzer-*.whl"))
if len(wheels) != 1:
    sys.exit(f"expected one wheel, found {wheels}")
(out / "manifest.json").write_text(json.dumps({"wheel": wheels[0].name}))
(out / ".nojekyll").write_text("")
print(f"site built in {out} with {wheels[0].name}")
