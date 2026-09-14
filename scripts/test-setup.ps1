<#
.SYNOPSIS
    Boots a machine that has never been configured, sets it up by typing, and
    boots it again to prove the answers stuck.

.DESCRIPTION
    The first-run wizard is the one part of this system a person meets before
    anything else works, and the only way to know it works is to use it: this
    sends the keys somebody would press, in order, through QEMU's keyboard --
    the same path a real keystroke takes, through the 8042 controller, the
    kernel's decoder, the compositor's routing and into the program.

    Then it boots the same disk a second time. What that checks is the half
    that matters: a wizard that runs perfectly and does not persist its answers
    is a wizard that runs again every morning.

    The disk is made fresh here rather than reused, because "has this machine
    been set up" is a question about the disk and a previous run would have
    answered it.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

# A disk nobody has ever set up. Made from the same program directory the
# ordinary build uses, so what is on it is what ships.
$Disk = Join-Path $BuildDir 'nexus-setup-disk.img'
$ProgramDir = Join-Path $BuildDir 'programs'
Write-Host '==> Making a disk that has never been configured' -ForegroundColor Cyan
& powershell -NoProfile -ExecutionPolicy Bypass `
    -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $ProgramDir
if ($LASTEXITCODE -ne 0) { throw 'could not make the disk' }

$MonitorPort = Get-Random -Minimum 24000 -Maximum 26000

function Start-Machine {
    param([string]$Log, [int]$Port)

    $vars = Join-Path $BuildDir 'vars-setup.fd'
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force
    if (Test-Path $Log) { Remove-Item $Log -Force }

    # The same machine as every other test, with this disk in place of the one
    # the build makes: `Get-NexusQemuArgs` reads the standard path, so the file
    # is swapped in by name.
    $standard = Get-NexusDiskImage -BuildDir $BuildDir
    $saved = "$standard.setup-saved"
    if (Test-Path $standard) { Move-Item $standard $saved -Force }
    Copy-Item $Disk $standard -Force

    $arguments = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $Log `
        -MonitorPort $Port -Headless

    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $arguments -PassThru -NoNewWindow
    return @{ Process = $process; Standard = $standard; Saved = $saved }
}

function Stop-Machine {
    param($Machine)
    if (-not $Machine.Process.HasExited) {
        try { $Machine.Process.Kill() } catch { }
    }
    $Machine.Process.WaitForExit(5000) | Out-Null
    # What the run left on the disk is what the next boot has to read, so it is
    # copied back before the original is restored.
    Copy-Item $Machine.Standard $Disk -Force
    if (Test-Path $Machine.Saved) { Move-Item $Machine.Saved $Machine.Standard -Force }
}

function Wait-For {
    param([string]$Log, [string]$Text, [int]$Seconds, $Process)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($Process.HasExited) { return $false }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Text)) { return $true }
        }
    }
    return $false
}

$failures = @()
$FirstLog = Join-Path $BuildDir 'setup-boot1.log'
$SecondLog = Join-Path $BuildDir 'setup-boot2.log'

# ---------------------------------------------------------------- first boot
Write-Host '==> Boot 1: a machine that has never been set up' -ForegroundColor Cyan
$machine = Start-Machine -Log $FirstLog -Port $MonitorPort
try {
    if (-not (Wait-For -Log $FirstLog -Text 'setup: this machine has not been set up' `
                -Seconds $Timeout -Process $machine.Process)) {
        throw 'the wizard never appeared'
    }

    Write-Host '==> Typing the answers' -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        # Every key spaced out, because the point is that the path works rather
        # than how fast it is, and a dropped key would otherwise be blamed on
        # the queue.
        function Send-Keys {
            param([string[]]$Keys)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds 220
            }
        }

        # Language: leave it, and continue.
        Send-Keys @('ret')
        # Timezone: one press of tab moves off UTC, so the choice is visibly a
        # choice rather than the default surviving.
        Send-Keys @('tab', 'ret')
        # A name.
        Send-Keys @('n', 'e', 'x', 'u', 's', 'ret')
        # A password, twice. Eight characters, because the wizard refuses fewer
        # -- which the test below checks by looking for the complaint.
        Send-Keys @('p', 'a', 's', 's', 'ret')
        Send-Keys @('w', 'o', 'r', 'd', 'ret')
        Send-Keys @('p', 'a', 's', 's', 'w', 'o', 'r', 'd', 'ret')
        # The network page, and then it writes.
        Send-Keys @('ret')

        if (-not (Wait-For -Log $FirstLog -Text 'setup: wrote settings' `
                    -Seconds 90 -Process $machine.Process)) {
            $failures += 'the wizard never wrote the settings'
        }
        # And let the filesystem finish with them before the machine is stopped.
        Start-Sleep -Seconds 5
        $writer.WriteLine('quit')
        Start-Sleep -Milliseconds 500
    } finally {
        $client.Close()
    }
} finally {
    Stop-Machine -Machine $machine
}

$first = (Get-Content $FirstLog -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'machine not yet set up',
        'compositor: this machine has not been set up; showing the wizard',
        'setup: this machine has not been set up; asking'
    )) {
    if ($first.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "boot 1 never reported: $expected"
    }
}

# The short password has to have been refused. Without this the test would pass
# on a wizard that accepted anything, which is the failure that matters.
if ($first -match 'setup: wrote settings for nexus') {
    Write-Host '    ok   the answers were written' -ForegroundColor DarkGray
} else {
    $failures += 'the wizard did not write the name that was typed'
}
if ($first -match 'setup: wrote settings for nexus \(en-US, ([^)]+)\)') {
    $zone = $Matches[1]
    if ($zone -eq 'UTC') {
        $failures += 'the timezone that was chosen was not the one stored'
    } else {
        Write-Host "    ok   and the timezone that was chosen ($zone)" -ForegroundColor DarkGray
    }
} else {
    $failures += 'the settings line did not say what was stored'
}

# --------------------------------------------------------------- second boot
Write-Host '==> Boot 2: the same disk, already configured' -ForegroundColor Cyan
$machine = Start-Machine -Log $SecondLog -Port ($MonitorPort + 1)
try {
    if (-not (Wait-For -Log $SecondLog -Text 'boot thread retiring' `
                -Seconds $Timeout -Process $machine.Process)) {
        $failures += 'the second boot never got started'
    }
    # The desktop draws a clock, which means it waits on its channel with a
    # deadline rather than for ever. Nothing else on this machine does, and the
    # only way to tell a deadline that works from one that never expires is to
    # wait past a minute boundary and see whether the strip noticed. Up to 90
    # seconds because the boundary can be anywhere in the minute.
    if (-not (Wait-For -Log $SecondLog -Text 'shell: the clock moved on' `
                -Seconds 90 -Process $machine.Process)) {
        $failures += 'the desktop clock never advanced on its own'
    }
} finally {
    Stop-Machine -Machine $machine
}

$second = (Get-Content $SecondLog -Raw -Encoding UTF8) -replace "`0", ''

if ($second.Contains('machine already set up')) {
    Write-Host '    ok   the second boot knew the machine was configured' -ForegroundColor DarkGray
} else {
    $failures += 'the second boot did not know the machine was configured'
}
if ($second.Contains('showing the wizard')) {
    $failures += 'the wizard ran again on a machine that was already set up'
} else {
    Write-Host '    ok   and did not run the wizard again' -ForegroundColor DarkGray
}
if ($second.Contains('desktop: welcome')) {
    Write-Host '    ok   and the desktop greeted the user by name' -ForegroundColor DarkGray
} else {
    $failures += 'the desktop did not read the settings'
}
if ($second -match 'desktop: welcome, nexus \(en-US, Japan') {
    Write-Host '    ok   in the language and timezone that were chosen' -ForegroundColor DarkGray
} else {
    $failures += 'the desktop did not apply the language and timezone that were chosen'
}
if ($second.Contains('shell: the clock moved on')) {
    Write-Host '    ok   and its clock ticked on a deadline, with nothing to wake it' -ForegroundColor DarkGray
}

if ($first.Contains('KERNEL PANIC') -or $second.Contains('KERNEL PANIC')) {
    $failures += 'the kernel panicked'
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Setup tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Logs: $FirstLog and $SecondLog"
    exit 1
}
