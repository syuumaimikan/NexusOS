"""Extract pinned, production GB202/GB206 GSP bootstrap material; never run it."""
from pathlib import Path
import hashlib
import importlib.util
import json
import os
import re
import runpy
import struct

REPO = Path(__file__).resolve().parents[2]
REVISION = 'ca2d03e6fafd4d4b56b383497d931c25b283bfcf'
VERSION = '615.78.08'


def main():
    runpy.run_path(str(Path(__file__).with_name('fetch-nvidia.py')))
    source = REPO / 'build/nvidia' / ('open-gpu-kernel-modules-' + REVISION)
    if not re.search(r'^NVIDIA_VERSION = ' + re.escape(VERSION) + r'\s*$',
                     (source / 'version.mk').read_text(), re.M):
        raise ValueError('Source release does not match the pinned firmware version')
    output = REPO / 'build/nvidia/firmware'
    spec = importlib.util.spec_from_file_location('nvidia_extract', source / 'nouveau/extract-firmware-nouveau.py')
    extractor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(extractor)
    original = extractor.get_bytes

    def get_bytes(filename, archive, label):
        # R615 changed binary arrays from NvU8 to aligned NvU64. The pinned
        # extractor still only recognizes NvU8; retain its binary file layout
        # and adapt the bindata reader without editing downloaded source.
        text = Path(filename).read_text()
        symbol = archive + '_BINDATA_LABEL_' + label.upper() + '_data'
        match = re.search(r'static BINDATA_CONST NV_DECLARE_ALIGNED\(NvU64, 8\)\s+' +
                          re.escape(symbol) + r'\[\]\s*=\s*\{(.*?)\};', text, re.S)
        if not match:
            return original(filename, archive, label)
        before = text[:match.start()]
        size = int(re.findall(r'DATA SIZE \(bytes\): (\d+)', before)[-1])
        compressed = re.findall(r'COMPRESSION: (YES|NO)', before)[-1] == 'YES'
        stored = int(re.findall(r'COMPRESSED SIZE \(bytes\): (\d+)', before)[-1]) if compressed else size
        if not 0 < size <= 1024 * 1024 or not 0 < stored <= 1024 * 1024:
            raise ValueError('Unbounded bindata image')
        words = re.findall(r'0x([0-9a-fA-F]+)ULL', match.group(1))
        raw = b''.join(struct.pack('<Q', int(word, 16)) for word in words)
        if len(raw) != (stored + 7) // 8 * 8 or any(raw[stored:]):
            raise ValueError('Bindata size/padding mismatch: ' + symbol)
        raw = raw[:stored]
        if compressed:
            # The trusted pinned source contains a raw deflate stream with a
            # gzip trailer, as documented by NVIDIA's own extractor.
            import zlib
            decoder = zlib.decompressobj(31)
            raw = decoder.decompress(b'\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03' + raw, size + 1)
            if not decoder.eof or decoder.unused_data or len(raw) != size:
                raise ValueError('Compressed bindata size/trailer mismatch')
        if len(raw) != size:
            raise ValueError('Bindata size mismatch')
        return raw

    extractor.get_bytes = get_bytes
    extractor.outputpath = str(output.resolve())
    extractor.version = VERSION
    old_cwd = Path.cwd()
    try:
        os.chdir(source)
        extractor.gsp_bootloader('gb202', 'prod')
        extractor.fmc('gb202', 'Prod', True)
    finally:
        os.chdir(old_cwd)
    bundle = REPO / 'build/nvidia/bundle'
    bundle.mkdir(parents=True, exist_ok=True)
    firmware = output / 'nvidia/gb202/gsp'
    records = {}
    for src, name in [('bootloader-' + VERSION + '.bin', 'GSPBOOT.BIN'), ('fmc-' + VERSION + '.bin', 'FMC.BIN')]:
        data = (firmware / src).read_bytes()
        records[name] = {'size': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
        (bundle / name).write_bytes(data)
    license_source = source / 'src/nvidia/generated/g_bindata_kgspGetBinArchiveGspRmBoot_GB202.c'
    notice = license_source.read_text().split('*/', 1)[0] + '*/\n'
    (bundle / 'LICENSE.TXT').write_text(notice, encoding='utf-8')
    manifest = {'source_revision': REVISION, 'version': VERSION, 'family': 'GB202/GB206',
                'production': True, 'gsp_rm_present': False, 'files': records}
    (bundle / 'FWINFO.TXT').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(manifest, indent=2))
    print('Bootstrap material only: full GSP-RM firmware and GPU submission remain required.')


if __name__ == '__main__':
    main()
