"""Package an already-built Rust target using only Python's standard library."""

import argparse
import hashlib
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import tomllib
import zipfile


TARGETS = {
    "x86_64-unknown-linux-musl": "linux-amd64-musl",
    "aarch64-unknown-linux-musl": "linux-arm64-musl",
    "x86_64-apple-darwin": "macos-amd64",
    "aarch64-apple-darwin": "macos-arm64",
    "x86_64-pc-windows-msvc": "windows-amd64",
    "aarch64-pc-windows-msvc": "windows-arm64",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", choices=TARGETS, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    with (root / "Cargo.toml").open("rb") as manifest:
        package = tomllib.load(manifest)["package"]

    name = f"{package['name']}-v{package['version']}-{TARGETS[args.target]}"
    windows = "-windows-" in args.target
    binary_name = package["name"] + (".exe" if windows else "")
    binary = root / "target" / args.target / "release" / binary_name
    if not binary.is_file():
        parser.error(f"release binary not found: {binary}")

    output = root / "dist"
    output.mkdir(exist_ok=True)
    archive = output / (name + (".zip" if windows else ".tar.gz"))
    with tempfile.TemporaryDirectory(prefix="package-", dir=root / "target") as temporary:
        staging = Path(temporary) / name
        staging.mkdir()
        for source in [binary, root / "config.example.toml", root / "README.md"]:
            shutil.copy2(source, staging / source.name)
        if windows:
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as bundle:
                for source in sorted(staging.iterdir()):
                    bundle.write(source, f"{name}/{source.name}")
        else:
            # Preserve the executable bit inside the tarball, including after artifact download.
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(staging, arcname=name)

    with archive.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    checksum = archive.with_name(archive.name + ".sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    print(f"Created {archive.name}")
    print(f"SHA256 {digest}")
    if github_output := os.environ.get("GITHUB_OUTPUT"):
        with open(github_output, "a", encoding="utf-8") as stream:
            stream.write(f"name={name}\n")


if __name__ == "__main__":
    main()
