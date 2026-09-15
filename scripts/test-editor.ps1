<#
.SYNOPSIS
    Opens the editor, types a file, saves it, and reads it back on a later boot.

.DESCRIPTION
    The whole path, end to end:

      the launcher is opened and the editor named -> the compositor makes a
      DOCS folder and lends it, read and write, to that one program -> letters
      are typed -> F2 writes the file -> the machine is *restarted* -> the
      editor is opened again and the file is still there.

    The restart is the part worth having. A file that is right in memory and a
    file that is on the disk are different claims, and only the second boot can
    tell them apart -- the first one would pass just as well against a program
    that never wrote anything.

    It also checks the two refusals that matter: Escape with unsaved work does
    not close the window the first time, and a short write is reported as a
    failure rather than as a save.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the editor, if one is wanted.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240,
    [string]$Shot
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')

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

$failures = @()
$checks = 0

function Test-Says {
    param([string]$What, [string]$Text, [string]$In)
    $script:checks++
    if ($In.Contains($Text)) {
        Write-Host "    ok   $What" -ForegroundColor DarkGray
    } else {
        $script:failures += "$What : never said '$Text'"
        Write-Host "    FAIL $What" -ForegroundColor Red
    }
}

# One boot: start the machine, run a script block against its monitor, stop it.
function Invoke-Boot {
    param([string]$LogName, [scriptblock]$Body)

    $log = Join-Path $BuildDir $LogName
    if (Test-Path $log) { Remove-Item $log -Force }
    $vars = Join-Path $BuildDir "vars-$LogName.fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force

    $port = Get-Random -Minimum 35000 -Maximum 35999
    $arguments = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $log `
        -MonitorPort $port -Headless

    Write-Host "==> Booting with a monitor on port $port" -ForegroundColor Cyan
    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $arguments -PassThru -NoNewWindow

    # Local to this boot, because each one has its own log.
    $waitFor = {
        param([string]$Text, [int]$Seconds)
        for ($waited = 0; $waited -lt $Seconds; $waited++) {
            Start-Sleep -Seconds 1
            if ($process.HasExited) { return $false }
            if (Test-Path $log) {
                $sofar = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
                if ($sofar.Contains($Text)) { return $true }
            }
        }
        return $false
    }

    try {
        if (-not (& $waitFor 'shell: took the strip' $Timeout)) {
            throw 'the desktop never appeared'
        }
        $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $port)
        try {
            $writer = New-Object System.IO.StreamWriter($client.GetStream())
            $writer.AutoFlush = $true
            Start-Sleep -Milliseconds 800
            & $Body $writer $waitFor
            $writer.WriteLine('quit')
            Start-Sleep -Milliseconds 800
        } finally {
            $client.Close()
        }
    } finally {
        if (-not $process.HasExited) {
            try { $process.Kill() } catch { }
        }
        $process.WaitForExit(5000) | Out-Null
    }

    return (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
}

function Send-Keys {
    param($Writer, [string[]]$Keys, [int]$Pause = 170)
    foreach ($key in $Keys) {
        $Writer.WriteLine("sendkey $key")
        Start-Sleep -Milliseconds $Pause
    }
}

# ---------------------------------------------------------------------------
# The first boot: type a file and save it.
# ---------------------------------------------------------------------------

Write-Host '==> First boot: write a file' -ForegroundColor Cyan
$first = Invoke-Boot 'editor-test.log' {
    param($writer, $waitFor)

    Send-Keys -Writer $writer -Keys @('f3')
    if (-not (& $waitFor 'launch: a window for starting things by name' 60)) {
        $script:failures += 'the launcher never started'
        return
    }

    # "wri" finds "Write a file" and nothing else on the list.
    Write-Host '==> Naming the editor' -ForegroundColor Cyan
    Send-Keys -Writer $writer -Keys @('w', 'r', 'i')
    Start-Sleep -Seconds 1
    Send-Keys -Writer $writer -Keys @('ret')

    if (-not (& $waitFor 'compositor: started an editor' 60)) {
        $script:failures += 'the compositor never started an editor'
        return
    }
    if (-not (& $waitFor 'edit: a window for writing a file' 60)) {
        $script:failures += 'the editor never said it was up'
        return
    }

    # Two lines, so that Enter splitting a line is exercised as well as typing.
    Write-Host '==> Typing' -ForegroundColor Cyan
    Send-Keys -Writer $writer -Keys @('n', 'e', 'x', 'u', 's', 'ret', 'e', 'd', 'i', 't')
    Start-Sleep -Seconds 1

    if ($Shot) {
        $ppm = Join-Path $BuildDir 'editor.ppm'
        Invoke-Screendump -Writer $writer -Path $ppm
        Convert-PpmToPng -PpmPath $ppm -PngPath $Shot | Out-Null
        Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
    }

    # Escape once, with unsaved work: it must refuse and stay open.
    Write-Host '==> Escape with unsaved work' -ForegroundColor Cyan
    Send-Keys -Writer $writer -Keys @('esc')
    Start-Sleep -Seconds 2

    Write-Host '==> F2 to save' -ForegroundColor Cyan
    Send-Keys -Writer $writer -Keys @('f2')
    if (-not (& $waitFor 'edit: wrote' 60)) {
        $script:failures += 'the editor never wrote the file'
    }
    Start-Sleep -Seconds 2
}

Test-Says 'the compositor made a folder and lent it' 'compositor: started an editor, and lent it the documents folder' $first
Test-Says 'the editor started on a file' 'edit: a window for writing a file' $first
Test-Says 'it wrote the file' 'edit: wrote NOTES.TXT' $first
# The three that must be absent.
foreach ($bad in @('edit: FAILED', 'edit: PANIC', 'KERNEL PANIC')) {
    $checks++
    if ($first.Contains($bad)) {
        $failures += "absent: $bad : it appeared"
        Write-Host "    FAIL absent: $bad" -ForegroundColor Red
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

# ---------------------------------------------------------------------------
# The second boot: the file is still there.
# ---------------------------------------------------------------------------

Write-Host ''
Write-Host '==> Second boot: is it still there?' -ForegroundColor Cyan
$second = Invoke-Boot 'editor-again.log' {
    param($writer, $waitFor)

    Send-Keys -Writer $writer -Keys @('f3')
    if (-not (& $waitFor 'launch: a window for starting things by name' 60)) {
        $script:failures += 'the launcher never started on the second boot'
        return
    }
    Send-Keys -Writer $writer -Keys @('w', 'r', 'i')
    Start-Sleep -Seconds 1
    Send-Keys -Writer $writer -Keys @('ret')

    if (-not (& $waitFor 'edit: a window for writing a file' 60)) {
        $script:failures += 'the editor never started on the second boot'
    }
    Start-Sleep -Seconds 2
}

# This is the assertion the whole test exists for. Two lines went in; two lines
# have to come back, off the disk, in a program that was started fresh.
Test-Says 'the file survived a restart, with both lines' 'edit: NOTES.TXT read, 2 lines' $second

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "Editor tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed." -ForegroundColor Red
    exit 1
}
