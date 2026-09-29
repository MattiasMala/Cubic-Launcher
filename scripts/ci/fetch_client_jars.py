#!/usr/bin/env python3
"""Put the vanilla client.jar of each version the tests read where they read it.

The tests that read real jars (see src-tauri/src/test_client_jars.rs) look in
$HOME/.local/share/com.cubic.launcher/cache/minecraft/<version>/client.jar.
CI has no such cache, so this fetches the jars the way the launcher does
(minecraft_downloader.rs): version_manifest_v2.json -> the version's
version.json -> downloads.client, sha1-checked. The jar is written to a
.part next to the target and renamed, so a cut download never looks whole.

Standard library only. Exits non-zero on any failure: in CI a jar that did not
arrive must stop the build here, not surface later as a skipped test.
"""

import argparse
import hashlib
import json
import sys
import time
import urllib.request
from pathlib import Path

VERSION_MANIFEST_URL = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"
DEFAULT_DEST = Path.home() / ".local/share/com.cubic.launcher/cache/minecraft"
# The versions the tests read. The CI cache key hashes this file, so changing
# the list here is what makes CI fetch again.
REQUIRED_VERSIONS = ("1.20.1", "26.3")
TIMEOUT_SECONDS = 60


def fetch_json(url: str) -> dict:
    with urllib.request.urlopen(url, timeout=TIMEOUT_SECONDS) as response:
        return json.load(response)


def download_verified(url: str, sha1: str, size: int, target: Path) -> int:
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + ".part")
    digest = hashlib.sha1()
    written = 0
    with urllib.request.urlopen(url, timeout=TIMEOUT_SECONDS) as response, open(partial, "wb") as out:
        while chunk := response.read(1 << 20):
            digest.update(chunk)
            out.write(chunk)
            written += len(chunk)
    if written != size or digest.hexdigest() != sha1:
        partial.unlink(missing_ok=True)
        raise RuntimeError(
            f"{url}: got {written} bytes with sha1 {digest.hexdigest()}, "
            f"expected {size} bytes with sha1 {sha1}"
        )
    partial.replace(target)
    return written


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dest", type=Path, default=DEFAULT_DEST)
    args = parser.parse_args()

    started = time.monotonic()
    manifest = fetch_json(VERSION_MANIFEST_URL)
    by_id = {entry["id"]: entry for entry in manifest["versions"]}

    total = 0
    for version in REQUIRED_VERSIONS:
        entry = by_id.get(version)
        if entry is None:
            print(f"{version}: not in {VERSION_MANIFEST_URL}", file=sys.stderr)
            return 1
        client = fetch_json(entry["url"])["downloads"]["client"]
        target = args.dest / version / "client.jar"
        written = download_verified(client["url"], client["sha1"], client["size"], target)
        total += written
        print(f"{version}: {written} bytes, sha1 {client['sha1']} -> {target}")

    print(f"total: {total} bytes in {time.monotonic() - started:.1f}s")
    return 0


if __name__ == "__main__":
    sys.exit(main())
