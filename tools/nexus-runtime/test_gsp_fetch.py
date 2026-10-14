"""Archive extraction regression checks; no downloads or executable payloads."""
import importlib.util
import io
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest.mock import patch
from compression.zstd import compress

spec = importlib.util.spec_from_file_location('fetch_gsp',Path(__file__).with_name('fetch-gsp.py'))
fetch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fetch)


class ArchiveTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='nexus-gsp-fetch-')
        self.old_root = fetch.ROOT
        fetch.ROOT = Path(self.temp.name)

    def tearDown(self):
        fetch.ROOT = self.old_root
        self.temp.cleanup()

    def archive(self, members):
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw,mode='w') as package:
            for name,data,kind in members:
                entry = tarfile.TarInfo(name)
                entry.type = kind
                entry.size = len(data) if kind == tarfile.REGTYPE else 0
                entry.linkname = '../../outside'
                package.addfile(entry,io.BytesIO(data) if entry.size else None)
        path = fetch.ROOT / 'fixture.run'
        path.write_bytes(b'version_string=615.78.08\nskip=3\n'+compress(raw.getvalue()))
        return path

    def good(self):
        return [(name,('data:'+name).encode(),tarfile.REGTYPE) for name in fetch.WANTED]

    def test_extracts_only_fixed_names_without_running_or_traversing(self):
        members = self.good()+[('../../outside',b'bad',tarfile.REGTYPE),
                               ('nvidia-installer',b'never run',tarfile.REGTYPE)]
        destination = fetch.extract(self.archive(members))
        for name,output in fetch.WANTED.items():
            self.assertEqual((destination/output).read_bytes(),('data:'+name).encode())
        self.assertEqual(sorted(p.name for p in destination.iterdir()),
                         sorted(list(fetch.WANTED.values())+['GSPINFO.TXT']))

    def test_duplicate_and_symlink_firmware_are_refused(self):
        for extra in [('firmware/gsp_ga10x.bin',b'duplicate',tarfile.REGTYPE),
                      ('firmware/gsp_ga10x.bin',b'',tarfile.SYMTYPE)]:
            with self.assertRaises(ValueError):
                fetch.extract(self.archive([extra]+self.good()))

    def test_missing_files_and_bad_preamble_are_refused(self):
        with self.assertRaises(ValueError):
            fetch.extract(self.archive(self.good()[:1]))
        for preamble in [b'version_string=wrong\nskip=3\n',
                         b'version_string=615.78.08\nskip=999999\n',
                         b'version_string=615.78.08\nskip=3\nno zstd']:
            with self.assertRaises(ValueError): fetch.payload(io.BytesIO(preamble))

    def test_modified_cached_package_is_refused_without_network(self):
        archive = fetch.ROOT / fetch.PACKAGE
        archive.write_bytes(b'broken cache')
        with self.assertRaisesRegex(ValueError,'Cached NVIDIA package'):
            fetch.download()
        with patch.object(fetch,'PACKAGE_SIZE',archive.stat().st_size):
            with self.assertRaisesRegex(ValueError,'SHA256 mismatch'):
                fetch.download()


if __name__ == '__main__':
    unittest.main()
