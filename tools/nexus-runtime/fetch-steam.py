"""Cache the official Steam installer and named bootstrap files for diagnostics.

This does not install a working Steam client or accept any Steam agreement.
It never executes the package's scripts and writes no host symlinks.
"""
import hashlib
import io
import json
from pathlib import Path
import tarfile
import urllib.request

URL = "https://cdn.fastly.steamstatic.com/client/installer/steam.deb"
SHA256 = "765aba9a0ed339a50226ceb614fcc9879a991ba184098bc8de920efb12c714a4"
VERSION = "1:1.0.0.87"
ROOT = Path(__file__).resolve().parents[2] / "build/runtime/steam"
LIMIT = 64 * 1024 * 1024


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
        if length < 0 or end > len(data):
            raise ValueError("truncated ar member")
        yield header[:16].decode("ascii").strip().rstrip("/"), data[begin:end]
        at = end + length % 2


def regular(package, name, limit=LIMIT):
    member = package.getmember(name)
    if not member.isfile() or member.size > limit:
        raise ValueError(f"unexpected member: {name}")
    stream = package.extractfile(member)
    contents = stream.read(limit + 1)
    if len(contents) != member.size:
        raise ValueError(f"invalid member length: {name}")
    return contents


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    archive = ROOT / "steam.deb"
    if not archive.exists():
        with urllib.request.urlopen(URL, timeout=60) as response:
            contents = response.read(LIMIT + 1)
        if len(contents) > LIMIT:
            raise ValueError("oversized Steam package")
        if hashlib.sha256(contents).hexdigest() != SHA256:
            raise ValueError("official installer changed; review the new version before pinning")
        archive.write_bytes(contents)
    contents = archive.read_bytes()
    if len(contents) > LIMIT or hashlib.sha256(contents).hexdigest() != SHA256:
        raise ValueError("Steam installer digest mismatch")
    members = dict(ar_members(contents))
    with tarfile.open(fileobj=io.BytesIO(members["control.tar.gz"]), mode="r:gz") as package:
        control = regular(package, "./control", 64 * 1024)
        if f"Version: {VERSION}\n".encode() not in control:
            raise ValueError("Steam package version mismatch")
        (ROOT / "control.txt").write_bytes(control)
    with tarfile.open(fileobj=io.BytesIO(members["data.tar.xz"]), mode="r:xz") as package:
        bootstrap = regular(package, "./usr/lib/steam/bootstraplinux_ubuntu12_32.tar.xz")
        (ROOT / "copyright.txt").write_bytes(regular(package, "./usr/share/doc/steam-launcher/copyright", 1024 * 1024))
    selected = {
        "steam": "ubuntu12_32/steam",
        "steam.sh": "steam.sh",
        "crashhandler.so": "ubuntu12_32/crashhandler.so",
        "libgcc_s.so.1": "ubuntu12_32/steam-runtime/lib/i386-linux-gnu/libgcc_s.so.1",
        "libstdc++.so.6": "ubuntu12_32/steam-runtime/usr/lib/i386-linux-gnu/libstdc++.so.6.0.21",
    }
    evidence = {"url": URL, "package_sha256": SHA256, "version": VERSION, "files": {}}
    with tarfile.open(fileobj=io.BytesIO(bootstrap), mode="r:xz") as package:
        for target, name in selected.items():
            data = regular(package, name)
            if target == "steam" and not data.startswith(b"\x7fELF\x01"):
                raise ValueError("expected an ELF32 Steam bootstrap")
            (ROOT / target).write_bytes(data)
            evidence["files"][target] = {"member": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    (ROOT / "manifest.json").write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    print(f"Official Steam bootstrap cached for diagnostics: {ROOT}")


if __name__ == "__main__":
    main()
