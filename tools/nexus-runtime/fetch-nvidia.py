"""Prepare pinned official Blackwell driver sources for the native GPU port."""
from pathlib import Path
import hashlib
import tarfile
import urllib.request

REVISION = 'ca2d03e6fafd4d4b56b383497d931c25b283bfcf'
SHA256 = '5ba52e867ec61c2ba07abdacdd3405de8a02e063828810e19c9f91be87c45800'
ROOT = Path(__file__).resolve().parents[2] / 'build/nvidia'
ROOT.mkdir(parents=True, exist_ok=True)
archive = ROOT / 'source.tar.gz'
if not archive.exists():
    url = f'https://codeload.github.com/NVIDIA/open-gpu-kernel-modules/tar.gz/{REVISION}'
    with urllib.request.urlopen(url, timeout=60) as response:
        data = response.read(128 * 1024 * 1024 + 1)
    if len(data) > 128 * 1024 * 1024:
        raise ValueError('Oversized NVIDIA source archive')
    archive.write_bytes(data)
if hashlib.sha256(archive.read_bytes()).hexdigest() != SHA256:
    raise ValueError('NVIDIA source checksum mismatch')
destination = ROOT / f'open-gpu-kernel-modules-{REVISION}'
if not (destination / 'README.md').exists():
    with tarfile.open(archive, 'r:gz') as package:
        package.extractall(ROOT, filter='data')
print(f'Pinned official GPU sources: {destination}')
