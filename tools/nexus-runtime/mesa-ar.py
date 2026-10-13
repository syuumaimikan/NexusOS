"""Use regular GNU archives: Zig's ELF linker cannot consume thin archives."""
from pathlib import Path
import subprocess
import sys

AR = "C:/Program Files/LLVM/bin/llvm-ar.exe"
BUILD = Path(__file__).resolve().parents[2] / "build/mesa/out"
args = sys.argv[1:]
if args == ["-h"]:
    result = subprocess.run([AR, "-h"], capture_output=True, text=True)
    print(result.stdout.replace("[T]", "[ ]"), end="")
    sys.exit(result.returncode)
if len(args) >= 2 and args[0].isalpha() and "r" in args[0]:
    args[0] = args[0].replace("T", "")
    archive = Path(args[1]).resolve()
    if not archive.is_relative_to(BUILD.resolve()) or archive.suffix != ".a":
        raise ValueError("archive destination is outside the Mesa build")
    if archive.exists():
        with archive.open("rb") as stream:
            thin = stream.read(8) == b"!<thin>\n"
        if thin:
            archive.unlink()  # Only this derived archive, never source or user data.
sys.exit(subprocess.call([AR, "--format=gnu"] + args))
