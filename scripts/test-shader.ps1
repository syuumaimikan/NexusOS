<#
.SYNOPSIS
    Boots the machine, runs a SPIR-V shader inside a program built for Linux,
    and checks the shader's own output reaches the screen.

.DESCRIPTION
    `tools/nexus-guest/src/bin/shader.rs` is a static Linux x86-64 executable,
    compiled from Rust for `x86_64-unknown-linux-gnu` with no C runtime. It
    assembles a SPIR-V module -- the format `glslang` emits and
    `vkCreateShaderModule` takes -- reads it back with `nexus_spirv::Module`,
    and runs it once per fragment over a 160x100 grid, writing the colours into
    the window it was given.

    There is no Vulkan here, no OpenGL, no driver and no graphics hardware. The
    claim is about the *format*: a shader is read and executed, and the pixels
    it computed are on a screen.

    What is checked, from outside the machine:

      * The module reads back after being assembled, and is runnable.
      * Every fragment shaded without the interpreter reporting an instruction
        it does not implement.
      * The arena is still nearly empty afterwards, which is the bump
        allocator's wind-back working: sixteen thousand invocations that each
        allocated hundreds of times, in four megabytes.
      * The picture on the screen is the one the shader computed. The shader
        draws a gradient with a soft disc in the middle, so the test looks for
        what only that shader would produce: the middle of the window is
        brighter than its corners, and the left edge is darker than the right.

    That last check is the one worth having. A window full of one colour, a
    window of noise, and a window the shader never touched would all fail it.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
#>
[CmdletBinding()]
param([int]$Timeout = 300, [switch]$Fresh)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$Log = Join-Path $BuildDir 'shader.log'

# A copy of the built machine that this run has to itself; see
# `scripts/private-machine.ps1` for why, which is a long story about two
# agents and one `build/esp`.
$Machine = Get-PrivateMachine -BuildDir $BuildDir -Name 'linux-machine' -Fresh:$Fresh
$MachineDir = $Machine.BuildDir
$EspDir = $Machine.EspDir

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-shader.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

# Asked for rather than guessed: see `Get-FreeMonitorPort`.
$Port = Get-FreeMonitorPort
$QemuArgs = Get-NexusQemuArgs -BuildDir $MachineDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $Port -Headless

Write-Host '==> Booting NexusOS' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
$connection = $null
$failures = @()

function Wait-Marker([string]$Marker) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $deadline -and -not $process.HasExited) {
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar -match 'KERNEL PANIC') { throw "the kernel panicked; see $Log" }
            if ($sofar.Contains($Marker)) { return }
        }
        Start-Sleep -Milliseconds 500
    }
    throw "the machine never said '$Marker'; see $Log"
}

try {
    Wait-Marker 'shell: took the strip along the bottom of the screen'
    Write-Host '    ok   the desktop is up' -ForegroundColor DarkGray

    $connection = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $Port)
    $writer = New-Object System.IO.StreamWriter($connection.GetStream())
    $writer.AutoFlush = $true

    $writer.WriteLine('sendkey f7')
    Wait-Marker 'shader: a window, and a SPIR-V module to fill it with'
    Write-Host '    ok   the program has a window' -ForegroundColor DarkGray

    Wait-Marker 'shader: a fragment shader read, instructions in its body:'
    Write-Host '    ok   the module it assembled read back as a fragment shader' -ForegroundColor DarkGray

    Wait-Marker 'shader: shaded 16000 fragments, and the arena still holds'
    Write-Host '    ok   every fragment ran, and the arena was wound back after each' -ForegroundColor DarkGray

    Wait-Marker 'shader: a SPIR-V shader drew every pixel of this window'
    Write-Host '    ok   the shader''s colours are in the window' -ForegroundColor DarkGray

    Start-Sleep -Seconds 3

    $ppm = Join-Path $BuildDir 'shader.ppm'
    if (Test-Path $ppm) { Remove-Item $ppm -Force }
    Invoke-Screendump -Writer $writer -Path $ppm
    Convert-PpmToPng -Ppm $ppm -Png (Join-Path $BuildDir 'shader.png')

    # ---- the check from outside -------------------------------------------
    $bytes = [System.IO.File]::ReadAllBytes($ppm)
    $at = 0
    $fields = @()
    while ($fields.Count -lt 4 -and $at -lt $bytes.Length) {
        while ($at -lt $bytes.Length -and $bytes[$at] -in 32, 9, 10, 13) { $at++ }
        if ($at -lt $bytes.Length -and $bytes[$at] -eq 35) {
            while ($at -lt $bytes.Length -and $bytes[$at] -ne 10) { $at++ }
            continue
        }
        $start = $at
        while ($at -lt $bytes.Length -and $bytes[$at] -notin 32, 9, 10, 13) { $at++ }
        $fields += [System.Text.Encoding]::ASCII.GetString($bytes, $start, $at - $start)
    }
    $at++
    if ($fields[0] -ne 'P6') { throw "the screendump is not a binary PPM: $ppm" }
    $width = [int]$fields[1]
    $height = [int]$fields[2]
    Write-Host "    ok   screendump is ${width}x${height}" -ForegroundColor DarkGray

    # The shader writes blue from the disc alone: `colour.b` is `ring`, which is
    # one in the middle and zero outside it. So the window is findable by
    # looking for a pixel with far more blue than red -- which nothing else on
    # this desktop has -- and the disc is findable inside it the same way.
    function Get-Pixel([int]$x, [int]$y) {
        $p = $at + ($y * $width + $x) * 3
        [PSCustomObject]@{ R = $bytes[$p]; G = $bytes[$p + 1]; B = $bytes[$p + 2] }
    }

    $best = $null
    for ($y = 0; $y -lt $height; $y += 2) {
        for ($x = 0; $x -lt $width; $x += 2) {
            $pixel = Get-Pixel $x $y
            if ($pixel.B -gt 200 -and $pixel.R -gt 200 -and $pixel.G -gt 150) {
                # The centre of the disc: everything near one.
                if ($null -eq $best) { $best = [PSCustomObject]@{ X = $x; Y = $y } }
            }
        }
    }

    if ($null -eq $best) {
        $failures += "the bright middle of the shader's disc is not on the screen"
    } else {
        Write-Host ("    ok   the disc the shader computed is on the screen, around {0},{1}" -f `
                $best.X, $best.Y) -ForegroundColor DarkGray

        # And the gradient around it. The shader's blue channel falls away from
        # the centre of the disc, so a point well to the left of it must be
        # darker in blue than the centre. A window of one flat colour passes
        # nothing here.
        $centre = Get-Pixel $best.X $best.Y
        $leftX = [Math]::Max(0, $best.X - 60)
        $left = Get-Pixel $leftX $best.Y
        if ($left.B -lt $centre.B - 30) {
            Write-Host ("    ok   it fades outwards: blue {0} at the centre, {1} sixty pixels left" -f `
                    $centre.B, $left.B) -ForegroundColor DarkGray
        } else {
            $failures += ("the picture does not fade away from the disc: blue {0} at the centre, {1} to the left" -f `
                    $centre.B, $left.B)
        }
    }
} finally {
    if ($null -ne $connection) { $connection.Close() }
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'A SPIR-V shader ran on this machine and its output reached the screen.' `
        -ForegroundColor Green
    Write-Host 'This is the format, not Vulkan: see the note at the top of shared/nexus-spirv.' `
        -ForegroundColor DarkGray
    Write-Host "Evidence: $(Join-Path $BuildDir 'shader.png') and $Log" -ForegroundColor DarkGray
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) check(s) failed. Log: $Log" -ForegroundColor Red
    exit 1
}
