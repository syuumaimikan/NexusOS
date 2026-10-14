"""Read the host's AMD ACPI table; never access PCI/MMIO or change Windows."""
import ctypes
import os
from pathlib import Path


def main():
    if os.name != 'nt':
        raise RuntimeError('Host firmware capture requires Windows')
    get = ctypes.WinDLL('kernel32', use_last_error=True).GetSystemFirmwareTable
    get.argtypes = [ctypes.c_uint32, ctypes.c_uint32, ctypes.c_void_p, ctypes.c_uint32]
    get.restype = ctypes.c_uint32
    provider = int.from_bytes(b'ACPI', 'big')
    table = int.from_bytes(b'IVRS', 'little')
    size = get(provider, table, None, 0)
    if not 48 <= size <= 1024 * 1024:
        raise RuntimeError('IVRS unavailable/size rejected: ' + str(size) +
                           ', Windows error=' + str(ctypes.get_last_error()))
    buffer = ctypes.create_string_buffer(size)
    if get(provider, table, buffer, size) != size:
        raise RuntimeError('Firmware table size changed during capture')
    data = buffer.raw
    if data[:4] != b'IVRS' or int.from_bytes(data[4:8], 'little') != size or sum(data) % 256:
        raise ValueError('Host IVRS header/checksum rejected')
    output = Path(__file__).resolve().parents[2] / 'build/amdvi-host-ivrs.bin'
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_bytes(data)
    print('Read-only host IVRS capture:', size, 'bytes ->', output)


if __name__ == '__main__':
    main()
