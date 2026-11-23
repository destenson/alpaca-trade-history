"""Assemble the static GitHub Pages site into ./site (or the directory given as argv[1]).

site/
  index.html, app.js, style.css      <- web/ (index.html references app.js / style.css with ?v=<build>)
  alpaca_analyzer-<ver>-0<build>-py3-none-any.whl   <- this package, installed in the browser by micropip
  manifest.json                      <- {"wheel": "<wheel filename>", "build": "<build>"}

<build> is a hash of everything deployed, so every deployment has new file names: browsers can't serve a
stale cached copy, and the running page can tell (via manifest.json) that a newer version is live.
"""
import hashlib
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
h = hashlib.sha256()
for f in sorted(p for p in out.rglob("*") if p.is_file()):
    h.update(f.name.encode())
    h.update(f.read_bytes())
build = h.hexdigest()[:12]
# wheel build tag (must start with a digit): a new file name per deployment
dist, ver, *tags = wheels[0].name[:-4].split("-")
wheel = f"{dist}-{ver}-0{build}-{'-'.join(tags[-3:])}.whl"
wheels[0].rename(out / wheel)
index = out / "index.html"
html = index.read_text()
for asset in ("app.js", "style.css"):
    if f'"{asset}"' not in html:
        sys.exit(f"index.html no longer references {asset}")
    html = html.replace(f'"{asset}"', f'"{asset}?v={build}"')
index.write_text(html)
(out / "manifest.json").write_text(json.dumps({"wheel": wheel, "build": build}))
(out / ".nojekyll").write_text("")
print(f"site built in {out}: build {build}, {wheel}")
