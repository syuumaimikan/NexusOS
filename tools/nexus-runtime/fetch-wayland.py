"""Pinned upstream binaries/headers; only named regular members are extracted."""
import gzip
import hashlib
import io
from pathlib import Path
import sys
import tarfile
import urllib.request

PACKAGES = {
    "wayland-libs-client-1.23.1-r3": ("1772469c9f58e9ae06c03905a42844fb87cc060c16555c6182c4015fd19c46fa", {
        "usr/lib/libwayland-client.so.0.23.1": "libwayland-client.so.0"}),
    "libffi-3.4.8-r0": ("9a75cb9024693c1e52c3d8d7c9afb7c79e6e20f6c08df28effdb8dd816095083", {
        "usr/lib/libffi.so.8.1.4": "libffi.so.8"}),
    "wayland-dev-1.23.1-r3": ("f1c36a2b5293f9cfac82e43b7f8379b26bec3261b97b34796e35be82832d45a0", {
        **{f"usr/include/{name}": name for name in (
            "wayland-client-core.h", "wayland-client-protocol.h", "wayland-client.h", "wayland-util.h", "wayland-version.h")},
        "usr/bin/wayland-scanner": "wayland-scanner"}),
    "wayland-protocols-1.44-r0": ("a1cf9e5f6a0b70e0d914ffa3b84656e92645007d59f7f13acc679e7b8deea47b", {
        "usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml": "xdg-shell.xml"}),
    "libexpat-2.8.5-r0": ("771c18e6f7891a503a4664cfc09a3c4fb35a9b2fd2d4a643caf7206b792f5444", {
        "usr/lib/libexpat.so.1.12.5": "libexpat.so.1"}),
    "libxml2-2.13.9-r1": ("be361698c6f728f492b99550e99e898ae478940fe593d8c11b14c5f3d1b5a938", {
        "usr/lib/libxml2.so.2.13.9": "libxml2.so.2"}),
    "zlib-1.3.2-r1": ("af24e502645e2b392cae833856d776691ac21655362e5db69d7b89f147f76216", {
        "usr/lib/libz.so.1.3.2": "libz.so.1"}),
    "xz-libs-5.8.4-r0": ("37e169b7b400304fa7027cab79f9c1c7a280e83acf6c95463384b8abb7ebb254", {
        "usr/lib/liblzma.so.5.8.4": "liblzma.so.5"}),
}


def main():
    destination = Path(sys.argv[1]).resolve()
    destination.mkdir(parents=True, exist_ok=True)
    for name, (digest, members) in PACKAGES.items():
        archive = destination / (name + ".apk")
        if not archive.exists():
            url = f"https://dl-cdn.alpinelinux.org/alpine/v3.22/main/x86_64/{name}.apk"
            with urllib.request.urlopen(url, timeout=60) as response:
                archive.write_bytes(response.read(2 * 1024 * 1024))
        data = archive.read_bytes()
        if hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f"SHA256 mismatch: {name}")
        with tarfile.open(fileobj=io.BytesIO(gzip.decompress(data)), mode="r:", ignore_zeros=True) as package:
            for member_name, output_name in members.items():
                member = package.getmember(member_name)
                if not member.isfile() or member.size > 2 * 1024 * 1024:
                    raise ValueError(f"unexpected member: {member_name}")
                (destination / output_name).write_bytes(package.extractfile(member).read())
    print(f"Pinned Wayland libraries and build tools: {destination}")


if __name__ == "__main__":
    main()
