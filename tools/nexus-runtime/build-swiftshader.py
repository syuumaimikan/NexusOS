"""Cross-build the pinned upstream Vulkan CPU ICD for the Nexus Linux ABI."""
import argparse
import pathlib
import shutil
import subprocess
import re

REVISION = '0f87bb2d3742c2992fd1a7dcb2f051eebf79299c'
REPO = pathlib.Path(__file__).resolve().parents[2]
WORK = REPO / 'build/swiftshader'
SOURCE = WORK / 'source'
OUT = WORK / 'out'


def run(*args):
    subprocess.run([str(a) for a in args], check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--jobs', type=int, default=4)
    args = parser.parse_args()
    WORK.mkdir(parents=True, exist_ok=True)
    if not SOURCE.exists():
        run('git', 'clone', '--no-checkout', '--filter=blob:none',
            'https://github.com/google/swiftshader.git', SOURCE)
        run('git', '-C', SOURCE, 'checkout', '--detach', REVISION)
    head = subprocess.check_output(['git', '-C', str(SOURCE), 'rev-parse', 'HEAD'], text=True).strip()
    if head != REVISION:
        raise RuntimeError(f'SwiftShader revision differs: {head}')
    # Zig treats -mtune=generic as a target CPU. Its target already defaults to
    # baseline x86-64; omit the tuning-only flag without enabling extra ISA.
    cmakelists = SOURCE / 'CMakeLists.txt'
    original = cmakelists.read_text(encoding='utf-8-sig')
    adjusted = original.replace('set_cpp_flag("-mtune=generic")', '# Baseline target: no host CPU tuning')
    adjusted = adjusted.replace('set_cpp_flag("-march=x86-64")', 'set_cpp_flag("-mcpu=baseline")')
    if adjusted != original:
        cmakelists.write_text(adjusted, encoding='utf-8')
    # The bundled LLVM support configuration targets glibc. musl deliberately
    # has no mallinfo; use LLVM's existing fallback for allocation statistics.
    for config in (SOURCE / 'third_party/llvm-subzero/build/Linux').rglob('config.h'):
        original = config.read_text(encoding='utf-8')
        adjusted = original.replace('#define HAVE_MALLINFO 1', '/* musl: HAVE_MALLINFO unavailable */')
        adjusted = adjusted.replace('#define HAVE_MALLINFO2 1', '/* musl: HAVE_MALLINFO2 unavailable */')
        for unsupported in ('HAVE_LSEEK64', 'HAVE_EXECINFO_H', 'HAVE_BACKTRACE'):
            adjusted = adjusted.replace(f'#define {unsupported} 1', f'/* musl: {unsupported} unavailable */')
        if adjusted != original:
            config.write_text(adjusted, encoding='utf-8')
    zig = REPO / 'build/toolchains/zig-x86_64-windows-0.15.2/zig.exe'
    ninja = REPO / 'build/mesa/venv/Scripts/ninja.exe'
    llvm = pathlib.Path('C:/Program Files/LLVM/bin')
    cmake = shutil.which('cmake') or 'C:/Program Files/CMake/bin/cmake.exe'
    toolchain = WORK / 'linux-musl.cmake'
    def path(p):
        return str(p).replace('\\', '/')
    text = '\n'.join([
        'set(CMAKE_SYSTEM_NAME Linux)',
        'set(CMAKE_SYSTEM_PROCESSOR x86_64)',
        f'set(CMAKE_C_COMPILER "{path(zig)}")',
        'set(CMAKE_C_COMPILER_ARG1 cc)',
        f'set(CMAKE_CXX_COMPILER "{path(zig)}")',
        'set(CMAKE_CXX_COMPILER_ARG1 c++)',
        f'set(CMAKE_ASM_COMPILER "{path(zig)}")',
        'set(CMAKE_ASM_COMPILER_ARG1 cc)',
        'set(CMAKE_C_FLAGS_INIT "-target x86_64-linux-musl")',
        'set(CMAKE_CXX_FLAGS_INIT "-target x86_64-linux-musl")',
        'set(CMAKE_ASM_FLAGS_INIT "-target x86_64-linux-musl")',
        f'set(CMAKE_AR "{path(llvm / "llvm-ar.exe")}")',
        f'set(CMAKE_RANLIB "{path(llvm / "llvm-ranlib.exe")}")',
        'set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)',
        'set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)',
        'set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)',
        'set(CMAKE_FIND_ROOT_PATH_MODE_PACKAGE ONLY)',
        '',
    ])
    if not toolchain.exists() or toolchain.read_text() != text:
        toolchain.write_text(text)
    run(cmake, '-S', SOURCE, '-B', OUT, '-G', 'Ninja',
        f'-DCMAKE_TOOLCHAIN_FILE={path(toolchain)}',
        f'-DCMAKE_MAKE_PROGRAM={path(ninja)}',
        '-DCMAKE_BUILD_TYPE=Release', '-DREACTOR_BACKEND=Subzero',
        '-DCMAKE_LINK_DEPENDS_USE_LINKER=OFF',
        '-DCMAKE_SHARED_LINKER_FLAGS=-Wl,--undefined-version',
        '-DSWIFTSHADER_BUILD_TESTS=OFF', '-DSWIFTSHADER_BUILD_BENCHMARKS=OFF',
        '-DSWIFTSHADER_BUILD_PVR=OFF', '-DSWIFTSHADER_BUILD_WSI_XCB=OFF',
        '-DSWIFTSHADER_BUILD_WSI_WAYLAND=OFF', '-DSWIFTSHADER_WARNINGS_AS_ERRORS=OFF',
        '-DSWIFTSHADER_USE_GROUP_SOURCES=OFF')
    run(cmake, '--build', OUT, '--target', 'vk_swiftshader', '--parallel', args.jobs)
    library = OUT / 'Linux/libvk_swiftshader.so'
    if not library.exists():
        candidates = list(OUT.rglob('libvk_swiftshader.so'))
        if len(candidates) != 1:
            raise RuntimeError('Could not locate the Vulkan ICD')
        library = candidates[0]
    destination = REPO / 'build/runtime/libvk_swiftshader.so'
    run(llvm / 'llvm-strip.exe', '--strip-unneeded', library, '-o', destination)
    notices = [f'SwiftShader source revision {REVISION}; includes notices for unused upstream components.']
    groups = {}
    for file in sorted(SOURCE.rglob('*')):
        if not file.is_file() or '.git' in file.parts:
            continue
        if file.name.lower().startswith(('license', 'copying', 'notice')):
            notices += [str(file.relative_to(SOURCE)), file.read_text(encoding='utf-8', errors='replace')]
        elif file.suffix in ('.h', '.hpp', '.c', '.cc', '.cpp', '.inc'):
            with file.open(encoding='utf-8', errors='replace') as stream:
                prefix = stream.read(32768).lstrip()
            # Parse only the leading comment, without a backtracking group of
            # repeated line comments (quadratic on large generated headers).
            if prefix.startswith('/*'):
                end = prefix.find('*/')
                comment = prefix[:end + 2] if end >= 0 else ''
            else:
                lines = []
                for line in prefix.splitlines():
                    if line.startswith('//') or not line.strip():
                        lines.append(line)
                    else:
                        break
                comment = '\n'.join(lines)
            if comment and re.search(r'copyright|license|permission', comment, re.I):
                groups.setdefault(comment, []).append(str(file.relative_to(SOURCE)))
    for notice, files in groups.items():
        notices += ['Source: ' + ', '.join(files), notice]
    for component in ('libcxx', 'libcxxabi', 'libunwind'):
        license_file = zig.parent / 'lib' / component / 'LICENSE.TXT'
        notices += ['Zig bundled ' + component, license_file.read_text(encoding='utf-8')]
    notice_file = REPO / 'build/runtime/swiftshader-notice.txt'
    notice_file.write_text('\n\n'.join(notices), encoding='utf-8')
    print(f'Pinned SwiftShader ICD: {destination} ({destination.stat().st_size} bytes)', flush=True)


if __name__ == '__main__':
    main()
