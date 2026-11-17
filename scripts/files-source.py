#!/usr/bin/env python3
"""Create the uflinux-files source bundle with byte-stable USTAR headers (no compressor).

The workspace is a Cargo workspace, so the bundle is the whole repository minus
build artefacts: `cargo build --frozen` needs `Cargo.toml`, `Cargo.lock`,
`rust-toolchain.toml` and every crate, and `package()` in the APKBUILD reads
`data/` and `LICENSE` out of the same tree.

Why a hand-built tar instead of `git archive`: abuild verifies the tarball
against a sha512 in the APKBUILD, so the bytes must be reproducible. That means
no compressor (gzip embeds a name and a timestamp), mode 0644 on every member,
no symlinks, sorted entries, and no mtime or uid/gid fields at all — USTAR
defaults them to zero, which is what makes two runs on two machines agree.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
NAME = "uflinux-files"
# `target` is the build output; `aports` contains the recipe/checksum that
# consumes the bundle, so including it would make the tarball depend on itself.
# Include only the build script needed inside the package build from `scripts`.
# `tmp` is the scratch directory the probes and harnesses write into: it is
# gitignored, and a digest that moved because a developer ran a probe would make
# `--check` fail on a clean checkout of an unchanged tree. No tracked path has a
# `tmp` component.
SKIP = {"target", "out", ".git", "__pycache__", ".cache", "aports", "tmp"}
INCLUDE = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "LICENSE", "README.md",
           "crates", "data", "docs", "po", "tests", "scripts/files-pgo-build.sh"]


def bundle():
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    contents = {}
    for name in INCLUDE:
        top = ROOT / name
        if not top.exists():
            continue
        paths = sorted(p for p in top.rglob("*") if p.is_file()) if top.is_dir() else [top]
        for path in paths:
            relative = path.relative_to(ROOT)
            if any(part in SKIP for part in relative.parts):
                continue
            if path.is_symlink():
                raise ValueError(f"Source bundles cannot contain symlinks: {relative}")
            contents[str(relative)] = path.read_bytes()
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w", format=tarfile.USTAR_FORMAT) as archive:
        for name, data in sorted(contents.items()):
            entry = tarfile.TarInfo(f"{NAME}-{version}/{name}")
            entry.size = len(data)
            entry.mode = 0o644
            archive.addfile(entry, io.BytesIO(data))
    return version, stream.getvalue()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--update", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    version, data = bundle()
    digest = hashlib.sha512(data).hexdigest()
    lock_path = ROOT / "config/sources.lock.json"
    recipe_path = ROOT / "aports/uflinux-files/APKBUILD"
    lock = json.loads(lock_path.read_text()) if lock_path.exists() else {}
    recipe = recipe_path.read_text()
    if args.update:
        lock[NAME] = {"version": version, "source_sha512": digest}
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        lock_path.write_text(json.dumps(lock, indent=2) + "\n")
        recipe = re.sub(r"^pkgver=.*$", "pkgver=" + version, recipe, flags=re.M)
        recipe = re.sub(
            r'^sha512sums="[^"]*"',
            lambda _: f'sha512sums="\n{digest}  {NAME}-{version}.tar\n"',
            recipe,
            flags=re.M,
        )
        recipe_path.write_text(recipe)
    elif args.check:
        want = {"version": version, "source_sha512": digest}
        if lock.get(NAME) != want or digest not in recipe:
            raise SystemExit(f"{NAME} source changed; run python3 scripts/files-source.py --update")
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_bytes(data)
    print(f"{digest}  {NAME}-{version}.tar")


if __name__ == "__main__":
    main()
