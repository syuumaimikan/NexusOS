# Shared staging for normal images and disposable runtime tests. Dot-source.
function Publish-PosixRuntime {
    param([Parameter(Mandatory)][string]$ProgramDir, [switch]$WithWayland, [switch]$WithMesa, [switch]$WithGlibc, [switch]$WithVulkan, [switch]$WithGlibc32)
    $RuntimeRepo = Split-Path -Parent $PSScriptRoot
    $RuntimeDir = Join-Path $RuntimeRepo 'build/runtime'
    $RuntimeEntries = @(
        @('musl-static', 'musl.lx', '/usr/bin/musl-probe'),
        @('musl-threads', 'mthread.lx', '/usr/bin/musl-threads'),
        @('musl-dynamic', 'mdyn.lx', '/usr/bin/musl-dynamic'),
        @('ld-musl-x86_64.so.1', 'muslld.lx', '/lib/ld-musl-x86_64.so.1'),
        @('libnexus-probe.so', 'mlib.lx', '/usr/lib/libnexus-probe.so')
    )
    if ($WithWayland) {
        $RuntimeEntries += @(
            @('wayland/libwayland-client.so.0', 'mwl.lx', '/usr/lib/libwayland-client.so.0'),
            @('wayland/libffi.so.8', 'mffi.lx', '/usr/lib/libffi.so.8'),
            @('wayland/wayland-client', 'wlc.lx', '/usr/bin/wayland-client')
        )
    }
    if ($WithMesa) {
        $RuntimeEntries += @(
            @('libOSMesa.so.8', 'mosm.lx', '/usr/lib/libOSMesa.so.8'),
            @('mesa-probe', 'mesa.lx', '/usr/bin/mesa-probe')
        )
        if ($WithWayland) { $RuntimeEntries[7][0] = 'wayland/mesa-wayland-client' }
    }
    if ($WithGlibc) {
        $RuntimeEntries += @(
            @('glibc/glibc-basic', 'gbase.lx', '/usr/bin/glibc-probe'),
            @('glibc/glibc-threads', 'gthread.lx', '/usr/bin/glibc-threads'),
            @('glibc/glibc-dynamic', 'gdyn.lx', '/usr/bin/glibc-dynamic'),
            @('glibc/libnexus-glibc-probe.so', 'glib.lx', '/usr/lib/libnexus-glibc-probe.so'),
            @('glibc/amd64/ld-linux-x86-64.so.2', 'gld.lx', '/lib64/ld-linux-x86-64.so.2')
        )
        $GlibcIndex = 0
        foreach ($GlibcName in @('libc.so.6','libm.so.6','libdl.so.2','libpthread.so.0','librt.so.1','libresolv.so.2','libnss_dns.so.2','libnss_files.so.2')) {
            $RuntimeEntries += ,@("glibc/amd64/$GlibcName", "gnu$GlibcIndex.lx", "/lib/x86_64-linux-gnu/$GlibcName")
            $GlibcIndex++
        }
    }
    if ($WithGlibc32) {
        $RuntimeEntries += @(
            @('glibc/i386/glibc-basic', 'g32base.lx', '/usr/bin/glibc32-probe'),
            @('glibc/i386/glibc-dynamic', 'g32dyn.lx', '/usr/bin/glibc32-dynamic'),
            @('glibc/i386/glibc-threads', 'g32thrd.lx', '/usr/bin/glibc32-threads'),
            @('glibc/i386/libnexus-glibc-probe.so', 'g32lib.lx', '/usr/lib/i386-linux-gnu/libnexus-glibc-probe.so'),
            @('glibc/i386/ld-linux.so.2', 'g32ld.lx', '/lib/ld-linux.so.2')
        )
        $GlibcIndex = 0
        foreach ($GlibcName in @('libc.so.6','libm.so.6','libdl.so.2','libpthread.so.0','librt.so.1','libresolv.so.2','libnss_dns.so.2','libnss_files.so.2')) {
            $RuntimeEntries += ,@("glibc/i386/$GlibcName", "gnu32$GlibcIndex.lx", "/lib/i386-linux-gnu/$GlibcName")
            $GlibcIndex++
        }
    }
    if ($WithVulkan) {
        $RuntimeEntries += @(
            @('libvk_swiftshader.so', 'vswift.lx', '/usr/lib/libvk_swiftshader.so'),
            @('vulkan-probe', 'vulkan.lx', '/usr/bin/vulkan-probe')
        )
    }
    $RuntimeEntries += ,@('musl-exit-group', 'mexit.lx', '/usr/bin/musl-exit-group')
    if ($WithGlibc) { $RuntimeEntries += ,@('glibc/glibc-exit-group', 'gexit.lx', '/usr/bin/glibc-exit-group') }
    if ($WithGlibc32) { $RuntimeEntries += ,@('glibc/i386/glibc-exit-group', 'g32exit.lx', '/usr/bin/glibc32-exit-group') }
    $RuntimeNotice = [IO.File]::ReadAllText((Join-Path $RuntimeRepo 'tools/nexus-runtime/notices/musl.txt'))
    if ($WithGlibc -or $WithGlibc32) { $RuntimeNotice += "`n`n" + [IO.File]::ReadAllText((Join-Path $RuntimeDir 'glibc/amd64/NOTICE')) }
    if ($WithVulkan) { $RuntimeNotice += "`n`n" + [IO.File]::ReadAllText((Join-Path $RuntimeDir 'swiftshader-notice.txt')) }
    if ($WithWayland) {
        foreach ($NoticeName in @('wayland', 'libffi')) {
            $RuntimeNotice += "`n`n" + [IO.File]::ReadAllText((Join-Path $RuntimeRepo "tools/nexus-runtime/notices/$NoticeName.txt"))
        }
    }
    if ($WithMesa) {
        $RuntimeNotice += "`n`n" + [IO.File]::ReadAllText((Join-Path $RuntimeDir 'mesa-notice.txt'))
    }
    [IO.File]::WriteAllText((Join-Path $ProgramDir 'mnotice.txt'), $RuntimeNotice, [Text.UTF8Encoding]::new($false))
    $RuntimeManifest = Join-Path $ProgramDir 'linux.lst'
    $RuntimeText = [IO.File]::ReadAllText($RuntimeManifest)
    foreach ($RuntimeEntry in $RuntimeEntries) {
        $RuntimeSource = Get-Item -LiteralPath (Join-Path $RuntimeDir $RuntimeEntry[0])
        if ($RuntimeSource.Length -gt 64MB) { throw "Runtime file exceeds NexusFS's 64 MiB limit: $($RuntimeSource.FullName)" }
        Copy-Item -LiteralPath (Join-Path $RuntimeDir $RuntimeEntry[0]) -Destination (Join-Path $ProgramDir $RuntimeEntry[1]) -Force
        # Exactly one entry per destination, including replacing the old fixture.
        $RuntimePattern = '(?m)^\S+\s+' + [regex]::Escape($RuntimeEntry[2]) + '\s*$'
        $RuntimeText = [regex]::Replace($RuntimeText, $RuntimePattern, '')
        $RuntimeText = $RuntimeText.TrimEnd() + "`nBIN/" + $RuntimeEntry[1].ToUpperInvariant() + ' ' + $RuntimeEntry[2] + "`n"
    }
    foreach ($RuntimeAlias in @('/lib/libc.so', '/usr/lib/libc.musl-x86_64.so.1')) {
        $RuntimeText = [regex]::Replace($RuntimeText, '(?m)^\S+\s+' + [regex]::Escape($RuntimeAlias) + '\s*$', '')
        $RuntimeText = $RuntimeText.TrimEnd() + "`nBIN/MUSLLD.LX $RuntimeAlias`n"
    }
    $RuntimeText = [regex]::Replace($RuntimeText, '(?m)^\S+\s+/usr/share/nexus-runtime/NOTICE\s*$', '')
    $RuntimeText = $RuntimeText.TrimEnd() + "`nBIN/MNOTICE.TXT /usr/share/nexus-runtime/NOTICE`n"
    [IO.File]::WriteAllText($RuntimeManifest, $RuntimeText, [Text.UTF8Encoding]::new($false))
}
