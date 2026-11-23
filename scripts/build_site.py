"""Assemble the static GitHub Pages site into ./site (or the directory given as argv[1]).

site/
  index.html, app.js, style.css      <- web/ (index.html references app.js / style.css with ?v=<build>)
  engine-<build>.wasm                <- the Rust analysis engine (engine/), compiled to WebAssembly
  manifest.json                      <- {"engine": "<wasm filename>", "build": "<build>"}

<build> is a hash of everything deployed, so every deployment has new file names: browsers can't serve a
stale cached copy, and the running page can tell (via manifest.json) that a newer version is live.

Needs a Rust toolchain with the wasm32-unknown-unknown target (``rustup target add wasm32-unknown-unknown``).
"""
import hashlib
import json
import shutil
import subprocess
import sys
from pathlib import Path

root = Path(__file__).resolve().parents[1]
out = Path(sys.argv[1] if len(sys.argv) > 1 else root / "site").resolve()
engine = root / "engine"
subprocess.run(["cargo", "build", "--release", "--lib", "--target", "wasm32-unknown-unknown"], cwd=engine, check=True)
wasm = engine / "target" / "wasm32-unknown-unknown" / "release" / "alpaca_engine.wasm"

if out.exists():
    shutil.rmtree(out)
shutil.copytree(root / "web", out)
h = hashlib.sha256()
for f in sorted([p for p in out.rglob("*") if p.is_file()] + [wasm]):
    h.update(f.name.encode())
    h.update(f.read_bytes())
build = h.hexdigest()[:12]
name = f"engine-{build}.wasm"
shutil.copy(wasm, out / name)
index = out / "index.html"
html = index.read_text()
for asset in ("app.js", "style.css"):
    if f'"{asset}"' not in html:
        sys.exit(f"index.html no longer references {asset}")
    html = html.replace(f'"{asset}"', f'"{asset}?v={build}"')
index.write_text(html)
(out / "manifest.json").write_text(json.dumps({"engine": name, "build": build}))
(out / ".nojekyll").write_text("")
print(f"site built in {out}: build {build}, {name} ({(out / name).stat().st_size / 1e6:.1f} MB)")
