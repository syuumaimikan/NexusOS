"""Extract pinned GSP firmware as data; never execute the NVIDIA installer."""
from pathlib import Path, PurePosixPath
import hashlib
import json
import re
import tarfile
import urllib.request

VERSION = '615.78.08'
PACKAGE = 'NVIDIA-Linux-x86_64-' + VERSION + '.run'
URL = 'https://us.download.nvidia.com/XFree86/Linux-x86_64/' + VERSION + '/' + PACKAGE
# Published by NVIDIA alongside this exact package (.run.sha256sum).
SHA256 = '3e3f6dddc2ee767a08185300af7be3cb286ece76698d2dde3524599782d07ffe'
PACKAGE_SIZE = 529834675
ROOT = Path(__file__).resolve().parents[2] / 'build/nvidia'
WANTED = {'firmware/gsp_ga10x.bin': 'GSPRM.BIN',
          'firmware/ucodes_ga10x.bin': 'UCODES.BIN', 'LICENSE': 'NVLIC.TXT'}


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def download():
    ROOT.mkdir(parents=True, exist_ok=True)
    archive = ROOT / PACKAGE
    if not archive.exists():
        partial = ROOT / (PACKAGE + '.part')
        try:
            with urllib.request.urlopen(URL, timeout=60) as response, partial.open('wb') as out:
                total = 0
                while block := response.read(1024 * 1024):
                    total += len(block)
                    if total > PACKAGE_SIZE:
                        raise ValueError('NVIDIA package exceeds pinned size')
                    out.write(block)
                if total != PACKAGE_SIZE:
                    raise ValueError('Truncated NVIDIA package')
            if digest(partial) != SHA256:
                raise ValueError('NVIDIA package SHA256 mismatch')
            partial.replace(archive)
        finally:
            partial.unlink(missing_ok=True)
    if archive.stat().st_size != PACKAGE_SIZE or digest(archive) != SHA256:
        raise ValueError('Cached NVIDIA package size/SHA256 mismatch')
    return archive


def payload(stream):
    # The pinned Makeself header contains a shell script and a Linux zstd
    # executable. Skip both as bytes; neither is ever executed or extracted.
    first = stream.read(4096)
    if b'version_string=' + VERSION.encode() + b'\n' not in first:
        raise ValueError('Unexpected NVIDIA package release')
    match = re.search(rb'^skip=(\d+)$', first, re.M)
    if not match or not 1 < int(match[1]) < 4096:
        raise ValueError('Invalid pinned package header')
    stream.seek(0)
    for _ in range(int(match[1]) - 1):
        if not stream.readline(1024 * 1024).endswith(b'\n') or stream.tell() > 1024 * 1024:
            raise ValueError('Oversized or truncated package preamble')
    start = stream.tell()
    if stream.read(4) != b'\x28\xb5\x2f\xfd':
        raise ValueError('Expected a zstd tar payload')
    stream.seek(start)


def extract(archive):
    # Python 3.14's standard zstd reader avoids executing bundled host code.
    from compression.zstd import ZstdFile
    destination = ROOT / 'gsp-bundle'
    destination.mkdir(parents=True, exist_ok=True)
    records = {}
    with archive.open('rb') as stream:
        payload(stream)
        with ZstdFile(stream, 'rb') as decoded, tarfile.open(fileobj=decoded, mode='r|') as package:
            for member in package:
                name = str(PurePosixPath(member.name))
                if name not in WANTED:
                    continue
                if name in records or not member.isfile() or not 0 < member.size <= 128 * 1024 * 1024:
                    raise ValueError('Invalid/duplicate firmware archive member: ' + name)
                contents = package.extractfile(member).read(member.size + 1)
                if len(contents) != member.size:
                    raise ValueError('Truncated firmware archive member')
                output = WANTED[name]
                (destination / output).write_bytes(contents)
                records[name] = {'output': output, 'size': len(contents),
                                 'sha256': hashlib.sha256(contents).hexdigest()}
    if set(records) != set(WANTED):
        raise ValueError('Pinned package is missing required firmware/license files')
    manifest = {'version': VERSION, 'url': URL, 'package_sha256': SHA256,
                'files': records, 'executed': False}
    (destination / 'GSPINFO.TXT').write_text(json.dumps(manifest, indent=2) + '\n')
    print(json.dumps(manifest, indent=2))
    return destination


if __name__ == '__main__':
    extract(download())
