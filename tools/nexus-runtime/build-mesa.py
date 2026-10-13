"""Cross-build an LLVM-free OSMesa/softpipe runtime in workspace-local caches."""
import hashlib
import os
import re
from pathlib import Path
import subprocess
import shutil
import sys
import tarfile
import urllib.request
import zipfile

REPO = Path(__file__).resolve().parents[2]
WORK = REPO / "build/mesa"
ZIG = REPO / "build/toolchains/zig-x86_64-windows-0.15.2/zig.exe"
SOURCE = WORK / "mesa-23.3.6"
VENV = WORK / "venv"


def write_if_changed(path, content):
    if not path.exists() or path.read_text(encoding="utf-8") != content:
        path.write_text(content, encoding="utf-8")


def runtime_notices():
    # Retain upstream copyright/license headers, including helper libraries and
    # LLVM's statically linked C++ runtime. This is deliberately a superset of
    # the selected driver, with source file names so each notice stays traceable.
    notices = ["Mesa 23.3.6 source license notices (includes unused source components).\n",
               (SOURCE / "docs/license.rst").read_text(encoding="utf-8")]
    groups = {}
    for root in [SOURCE / "src", SOURCE / "include"]:
        for source in sorted(root.rglob("*")):
            if source.suffix not in {".c", ".cpp", ".h", ".hpp", ".S"}:
                continue
            prefix = source.read_text(encoding="utf-8", errors="replace")[:32768].lstrip()
            comments = re.match(r"(?:(?:/\*.*?\*/|//[^\n]*)(?:\s*))+", prefix, re.S)
            if comments and re.search(r"copyright|license|permission", comments[0], re.I):
                groups.setdefault(comments[0].strip(), []).append(source.relative_to(SOURCE).as_posix())
    for notice, files in groups.items():
        notices += ["\nSource: " + ", ".join(files), notice]
    for component in ["libcxx", "libcxxabi", "libunwind"]:
        license_file = ZIG.parent / "lib" / component / "LICENSE.TXT"
        notices += ["\nZig bundled " + component, license_file.read_text(encoding="utf-8")]
    write_if_changed(REPO / "build/runtime/mesa-notice.txt", "\n\n".join(notices))


def fetch(url, path, digest):
    if not path.exists():
        with urllib.request.urlopen(url, timeout=60) as response:
            path.write_bytes(response.read(64 * 1024 * 1024))
    if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
        raise ValueError(f"checksum mismatch: {path.name}")


def main():
    WORK.mkdir(parents=True, exist_ok=True)
    if not ZIG.exists():
        raise ValueError("Run build-posix-runtime.ps1 first")
    archive = WORK / "mesa-23.3.6.tar.xz"
    fetch("https://archive.mesa3d.org/mesa-23.3.6.tar.xz", archive,
          "cd3d6c60121dea73abbae99d399dc2facaecde1a8c6bd647e6d85410ff4b577b")
    if not SOURCE.exists():
        with tarfile.open(archive) as package:
            package.extractall(WORK, filter="data")
    # A Windows flex executable can generate Linux code. Mesa 23.3 selects
    # --wincompat from the build host alone; select it from the target instead.
    meson_source = SOURCE / "meson.build"
    source_text = meson_source.read_text(encoding="utf-8")
    old_flex = "    prog_flex = [prog_flex, '--wincompat']"
    new_flex = "    if host_machine.system() == 'windows'\n" + old_flex + "\n    else\n      prog_flex = [prog_flex]\n    endif"
    if new_flex not in source_text:
        if source_text.count(old_flex) != 1:
            raise ValueError("unexpected upstream flex configuration")
        source_text = source_text.replace(old_flex, new_flex)
    old_cpp = "  prog_flex_cpp += '-D__STDC_VERSION__=199901'"
    new_cpp = "  if host_machine.system() == 'windows'\n" + old_cpp + "\n  endif"
    if new_cpp not in source_text:
        if source_text.count(old_cpp) != 1:
            raise ValueError("unexpected upstream C++ flex configuration")
        source_text = source_text.replace(old_cpp, new_cpp)
    if meson_source.read_text(encoding="utf-8") != source_text:
        meson_source.write_text(source_text, encoding="utf-8")
    osmesa_meson = SOURCE / "src/gallium/targets/osmesa/meson.build"
    osmesa_text = osmesa_meson.read_text(encoding="utf-8")
    old_version = "'-Wl,--version-script', join_paths(meson.current_source_dir(), 'osmesa.sym')"
    new_version = "'-Wl,--version-script=' + join_paths(meson.current_source_dir(), 'osmesa.sym')"
    if old_version in osmesa_text:
        osmesa_meson.write_text(osmesa_text.replace(old_version, new_version), encoding="utf-8")
    elif new_version not in osmesa_text:
        raise ValueError("unexpected upstream version-script configuration")
    flex = WORK / "winflexbison.zip"
    fetch("https://github.com/lexxmark/winflexbison/releases/download/v2.5.25/win_flex_bison-2.5.25.zip", flex,
          "8d324b62be33604b2c45ad1dd34ab93d722534448f55a16ca7292de32b6ac135")
    tools = WORK / "winflexbison"
    if not (tools / "win_flex.exe").exists():
        with zipfile.ZipFile(flex) as package:
            for name in package.namelist():
                if not (tools / name).resolve().is_relative_to(tools.resolve()):
                    raise ValueError("unexpected archive path")
            package.extractall(tools)
    python = VENV / "Scripts/python.exe"
    if not python.exists():
        subprocess.run([sys.executable, "-m", "venv", str(VENV)], check=True)
    python3 = VENV / "Scripts/python3.exe"
    if not python3.exists() or python3.read_bytes() != python.read_bytes():
        shutil.copyfile(python, python3)
    subprocess.run([str(python), "-m", "pip", "install", "meson==1.10.0", "ninja==1.11.1.3",
                    "mako==1.3.5", "packaging==24.2", "pyyaml==6.0.2", "MarkupSafe==3.0.4"], check=True)
    zig = ZIG.as_posix()
    cross = WORK / "cross.ini"
    ar_wrapper = (REPO / "tools/nexus-runtime/mesa-ar.py").as_posix()
    write_if_changed(cross, f"""[binaries]
c = ['{zig}', 'cc', '-target', 'x86_64-linux-musl']
cpp = ['{zig}', 'c++', '-target', 'x86_64-linux-musl']
ar = ['{python.as_posix()}', '{ar_wrapper}']
python3 = '{python.as_posix()}'
strip = 'C:/Program Files/LLVM/bin/llvm-strip.exe'
nm = 'C:/Program Files/LLVM/bin/llvm-nm.exe'
[host_machine]
system = 'linux'
cpu_family = 'x86_64'
cpu = 'x86_64'
endian = 'little'
[properties]
needs_exe_wrapper = true
""")
    native = WORK / "native.ini"
    write_if_changed(native, f"""[binaries]
c = ['{zig}', 'cc']
cpp = ['{zig}', 'c++']
ar = ['{python.as_posix()}', '{ar_wrapper}']
python3 = '{python.as_posix()}'
nm = 'C:/Program Files/LLVM/bin/llvm-nm.exe'
""")
    env = dict(os.environ)
    env["PATH"] = str(VENV / "Scripts") + os.pathsep + str(tools) + os.pathsep + env["PATH"]
    env["PYTHONUTF8"] = "1"
    build = WORK / "out"
    meson = [str(python), "-m", "mesonbuild.mesonmain"]
    if not (build / "build.ninja").exists():
        subprocess.run(meson + ["setup", str(build), str(SOURCE), "--cross-file", str(cross), "--native-file", str(native),
            "--buildtype=release", "--wrap-mode=nodownload", "-Dplatforms=[]", "-Dgallium-drivers=swrast",
            "-Dvulkan-drivers=[]", "-Dllvm=disabled", "-Ddraw-use-llvm=false", "-Dosmesa=true",
            "-Dglx=disabled", "-Degl=disabled", "-Dgbm=disabled", "-Dgles1=disabled", "-Dgles2=disabled",
            "-Dshared-glapi=disabled", "-Dzlib=disabled", "-Dzstd=disabled", "-Dshader-cache=disabled",
            "-Dvalgrind=disabled", "-Dlibunwind=disabled", "-Dlmsensors=disabled", "-Dbuild-tests=false"], env=env, check=True)
    subprocess.run(meson + ["compile", "-C", str(build), "-j", "4"], env=env, check=True)
    runtime_notices()
    print(f"OSMesa build: {build}")


if __name__ == "__main__":
    main()
