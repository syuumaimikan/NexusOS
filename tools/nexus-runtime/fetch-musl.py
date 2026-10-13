"""Fetch a pinned real musl loader; extract only explicitly named regular files."""
import gzip
import hashlib
import io
from pathlib import Path
import sys
import tarfile
import urllib.request

URL = "https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/musl-1.2.5-r12.apk"
SHA256 = "4990a5e0ba312e478f94cfe431a70efef1538004eb361c8ae424516848be45bb"


def main():
    destination = Path(sys.argv[1]).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    archive = destination / "musl.apk"
    if not archive.exists():
        with urllib.request.urlopen(URL, timeout=60) as response:
            archive.write_bytes(response.read(2 * 1024 * 1024))
    data = archive.read_bytes()
    if hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("musl package SHA256 mismatch")
    # APK v2 contains concatenated gzip streams and tar end markers.
    with tarfile.open(fileobj=io.BytesIO(gzip.decompress(data)), mode="r:", ignore_zeros=True) as package:
        loader = package.getmember("lib/ld-musl-x86_64.so.1")
        if not loader.isfile() or loader.size > 2 * 1024 * 1024:
            raise ValueError("unexpected musl loader member")
        contents = package.extractfile(loader).read()
        if not contents.startswith(b"\x7fELF\x02"):
            raise ValueError("loader is not ELF64")
        (destination / "ld-musl-x86_64.so.1").write_bytes(contents)
    print(f"Pinned Alpine musl loader: {destination}")


if __name__ == "__main__":
    main()
