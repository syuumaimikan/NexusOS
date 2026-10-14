"""Fetch Debian's pinned static i386 BusyBox shell for Linux diagnostics."""
import hashlib
import io
from pathlib import Path
import tarfile
import urllib.request

URL = "https://deb.debian.org/debian/pool/main/b/busybox/busybox-static_1.35.0-4+deb12u1+b1_i386.deb"
SHA256 = "725ef9e6fbac4ab79b353165b05005e8c140d6e42177ebbc202ff1e13ded8b7c"
ROOT = Path(__file__).resolve().parents[2] / "build/runtime/busybox"
LIMIT = 4 * 1024 * 1024


def ar_members(data):
    if data[:8] != b"!<arch>\n":
        raise ValueError("not a Debian archive")
    at = 8
    while at < len(data):
        header = data[at:at + 60]
        if len(header) != 60 or header[58:] != b"`\n":
            raise ValueError("invalid ar member")
        length = int(header[48:58])
        begin, end = at + 60, at + 60 + length
        if end > len(data):
            raise ValueError("truncated ar member")
        yield header[:16].decode("ascii").strip().rstrip("/"), data[begin:end]
        at = end + length % 2


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    archive = ROOT / "busybox-static_i386.deb"
    if not archive.exists():
        with urllib.request.urlopen(URL, timeout=60) as response:
            data = response.read(LIMIT + 1)
        if len(data) > LIMIT:
            raise ValueError("oversized BusyBox package")
        archive.write_bytes(data)
    data = archive.read_bytes()
    if len(data) > LIMIT or hashlib.sha256(data).hexdigest() != SHA256:
        raise ValueError("BusyBox package digest mismatch")
    members = dict(ar_members(data))
    payload = next(body for name, body in members.items() if name.startswith("data.tar."))
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:*") as package:
        binary = package.getmember("./bin/busybox")
        notice = package.getmember("./usr/share/doc/busybox-static/copyright")
        if not binary.isfile() or binary.size > 4 * 1024 * 1024:
            raise ValueError("unexpected BusyBox binary")
        if not notice.isfile() or notice.size > 1024 * 1024:
            raise ValueError("unexpected BusyBox license")
        contents = package.extractfile(binary).read()
        if not contents.startswith(b"\x7fELF\x01"):
            raise ValueError("expected a 32-bit BusyBox executable")
        (ROOT / "busybox").write_bytes(contents)
        (ROOT / "copyright.txt").write_bytes(package.extractfile(notice).read())
    print(f"Pinned Debian BusyBox static i386 shell: {ROOT}")


if __name__ == "__main__":
    main()
