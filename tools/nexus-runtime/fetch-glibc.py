"""Fetch pinned Debian glibc binaries; extract named regular files only."""
import hashlib
import io
from pathlib import Path
import sys
import tarfile
import urllib.request

PACKAGES = {
    "amd64": "ba4f88f73dbc3ae9055f3c20f4523bfdbaf1ad13ff95e258924f77d20b4fbedf",
    "i386": "76b12e06be66ec3fc2c1791d7d8cf34c2b828ed210e3913feeeb8edc9688f820",
}
VERSION = "2.36-9+deb12u14"


def ar_members(data):
    if data[:8] != b"!<arch>\n":
        raise ValueError("not a Debian ar archive")
    at = 8
    while at < len(data):
        header = data[at:at + 60]
        if len(header) != 60 or header[58:] != b"`\n":
            raise ValueError("invalid ar header")
        length = int(header[48:58])
        begin, end = at + 60, at + 60 + length
        if end > len(data):
            raise ValueError("truncated ar member")
        yield header[:16].decode("ascii").strip().rstrip("/"), data[begin:end]
        at = end + length % 2


def main():
    output = Path(sys.argv[1]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    for arch, digest in PACKAGES.items():
        archive = output / f"libc6_{VERSION}_{arch}.deb"
        if not archive.exists():
            url = f"https://deb.debian.org/debian/pool/main/g/glibc/{archive.name}"
            with urllib.request.urlopen(url, timeout=60) as response:
                data = response.read(16 * 1024 * 1024 + 1)
                if len(data) > 16 * 1024 * 1024:
                    raise ValueError("oversized glibc package")
                archive.write_bytes(data)
        data = archive.read_bytes()
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f"glibc {arch} checksum mismatch")
        payload = next(body for name, body in ar_members(data) if name.startswith("data.tar."))
        triplet = "x86_64-linux-gnu" if arch == "amd64" else "i386-linux-gnu"
        loader = "ld-linux-x86-64.so.2" if arch == "amd64" else "ld-linux.so.2"
        destination = output / arch
        destination.mkdir(exist_ok=True)
        names = [loader, "libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1",
                 "libresolv.so.2", "libnss_dns.so.2", "libnss_files.so.2"]
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:*") as package:
            for name in names:
                member = package.getmember(f"./lib/{triplet}/{name}")
                if not member.isfile() or member.size > 8 * 1024 * 1024:
                    raise ValueError(f"unexpected glibc member {name}")
                contents = package.extractfile(member).read()
                expected = b"\x7fELF\x02" if arch == "amd64" else b"\x7fELF\x01"
                if not contents.startswith(expected):
                    raise ValueError(f"invalid ELF class for {name}")
                (destination / name).write_bytes(contents)
            copyright_file = package.getmember("./usr/share/doc/libc6/copyright")
            if not copyright_file.isfile() or copyright_file.size > 1024 * 1024:
                raise ValueError("unexpected copyright member")
            (destination / "NOTICE").write_bytes(package.extractfile(copyright_file).read())
    print(f"Pinned Debian glibc {VERSION} (amd64 and i386): {output}")


if __name__ == "__main__":
    main()
