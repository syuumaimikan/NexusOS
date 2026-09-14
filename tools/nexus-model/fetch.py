#!/usr/bin/env python3
"""Fetch only pinned public model assets; never upload prompts or local data."""
import hashlib
import pathlib
import urllib.request

ROOT = pathlib.Path(__file__).resolve().parents[2]
DEST = ROOT / 'build' / 'models'
REVISION = '0bd21da7698eaf29a0d7de3992de8a46ef624add'
SOURCE_REVISION = '350e04fe35433e6d2941dce5a1f53308f87058eb'
FILES = {
    'stories260K.bin': (f'https://huggingface.co/karpathy/tinyllamas/resolve/{REVISION}/stories260K/stories260K.bin', 1056540, 'b0a507e7ad0f626624f17112325e66691f9076d622e1d3274d103d00299f2696'),
    'tok512.bin': (f'https://huggingface.co/karpathy/tinyllamas/resolve/{REVISION}/stories260K/tok512.bin', 6227, '037cb335abb25d1fa9e8ecae30ed2a3a8ace9302862ebcdc05d51a6bbb10c312'),
    'run.c': (f'https://raw.githubusercontent.com/karpathy/llama2.c/{SOURCE_REVISION}/run.c', None, '9c4f2d5c6ae01b71726d1cc37530d71e60bff0ec7cc012565f16a43c1ca658bd'),
    'LICENSE': (f'https://raw.githubusercontent.com/karpathy/llama2.c/{SOURCE_REVISION}/LICENSE', None, 'e87b912002f04cdfa837eaf7f70548ab79b9cb62a285495edf575f3da875aec4'),
}

def main():
    DEST.mkdir(parents=True, exist_ok=True)
    for name, (url, length, digest) in FILES.items():
        path = DEST / name
        if path.exists() and hashlib.sha256(path.read_bytes()).hexdigest() == digest:
            print(f'verified {name}')
            continue
        with urllib.request.urlopen(url, timeout=60) as response:
            data = response.read(2 * 1024 * 1024 + 1)
        if len(data) > 2 * 1024 * 1024 or (length is not None and len(data) != length) or hashlib.sha256(data).hexdigest() != digest:
            raise ValueError(f'invalid download: {name}')
        temporary = path.with_suffix(path.suffix + '.download')
        temporary.write_bytes(data)
        temporary.replace(path)
        print(f'verified {name}')

if __name__ == '__main__':
    main()
