#!/usr/bin/env python3
"""Developer tool: writes THIRD_PARTY_NOTICES.md (what is used, under which
license) and THIRD_PARTY_LICENSES.txt (the license texts shipped by each crate,
de-duplicated). Reads the real dependency graph from `cargo metadata`, so the
list cannot drift from what is built.

    python3 tools/gen_notices.py
"""
import hashlib, json, os, subprocess, sys

root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
meta = json.loads(subprocess.check_output(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=root))
pkgs = sorted((p for p in meta["packages"] if p["name"] != "synkflow"), key=lambda p: (p["name"], p["version"]))

NAMES = ("LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE")
texts, by_hash, rows, problems = [], {}, [], []
for p in pkgs:
    d = os.path.dirname(p["manifest_path"])
    found = []
    for f in sorted(os.listdir(d)):
        if f.upper().startswith(NAMES) and os.path.isfile(os.path.join(d, f)):
            try:
                found.append((f, open(os.path.join(d, f), encoding="utf-8", errors="replace").read()))
            except OSError:
                pass
    lic = p.get("license") or ("see " + str(p.get("license_file"))) or "UNKNOWN"
    rows.append((p["name"], p["version"], lic, p.get("repository") or ""))
    if not found:
        problems.append(f'{p["name"]} {p["version"]} ({lic})')
    for fname, body in found:
        h = hashlib.sha256(body.strip().encode()).hexdigest()
        by_hash.setdefault(h, {"body": body.strip(), "who": []})["who"].append(f'{p["name"]} {p["version"]}')

with open(os.path.join(root, "THIRD_PARTY_NOTICES.md"), "w") as out:
    out.write("# Third-party notices\n\nSynkflow is licensed under **GPL-3.0-only** (see `LICENSE`). It is built from the open-source\ncomponents below, each under its own license. Slint is used under its **GPL-3.0-only** option.\nThe license texts these packages ship are collected in `THIRD_PARTY_LICENSES.txt`.\n\nThis file is generated from the dependency graph by `tools/gen_notices.py`; it lists every crate in\n`Cargo.lock` across all supported platforms, so it is a superset of what one platform's binary contains.\n\n| Package | Version | License |\n|---|---|---|\n")
    for n, v, l, r in rows:
        out.write(f"| {n} | {v} | {l} |\n")
    if problems:
        out.write("\n## Packages that ship no separate license file\n\nTheir license is stated in their `Cargo.toml` (shown above):\n\n")
        for pr in problems:
            out.write(f"- {pr}\n")

with open(os.path.join(root, "THIRD_PARTY_LICENSES.txt"), "w") as out:
    out.write("License texts shipped by Synkflow's dependencies (de-duplicated).\n")
    for h, e in sorted(by_hash.items(), key=lambda kv: kv[1]["who"][0]):
        out.write("\n" + "=" * 78 + "\nUsed by: " + ", ".join(e["who"][:12]) + (f" and {len(e['who']) - 12} more" if len(e["who"]) > 12 else "") + "\n" + "=" * 78 + "\n\n" + e["body"] + "\n")
print(f"{len(rows)} packages, {len(by_hash)} distinct license texts, {len(problems)} without a license file")
