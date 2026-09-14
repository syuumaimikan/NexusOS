<#
.SYNOPSIS
    Boots the machine over and over, and reports the boots that did not finish.

.DESCRIPTION
    Some failures happen once in ten boots. A test suite that boots once per
    stage will meet them eventually, in a stage that has nothing to do with
    them, on a run somebody is waiting on -- which is the worst way to find out.

    This boots the staged image repeatedly with nothing else going on, waits for
    a line that only a machine which got all the way prints, and keeps the log
    of every boot that did not. What it is for is the answer to "how often?",
    which is the first question worth asking about a race.

    It does not build. Run build.ps1 first, so that every boot here is the same
    image and a failure is a property of the machine rather than of a rebuild.

.PARAMETER Count
    How many times to boot.

.PARAMETER Marker
    The line a finished boot prints.

.PARAMETER Timeout
    How long one boot may take before it counts as stuck, in seconds.

.PARAMETER Processors
    How many processors to give the machine. Races that need two cores need two.
#>
[CmdletBinding()]
param(
    [int]$Count = 20,
    # The display thread retiring is the last thing the kernel says on a boot
    # that worked, and it says it whether or not the machine has been set up --
    # which matters, because a fresh disk stops at the wizard and a soak that
    # waited for the desktop would call every one of those boots a hang.
    [string]$Marker = "display thread retiring",
    [int]$Timeout = 90,
    [int]$Processors = 4,
    # Keep the logs of the boots that worked too. Off by default because sixty
    # logs of a machine behaving are sixty megabytes of nothing.
    [switch]$Keep
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SoakDir = Join-Path $BuildDir 'soak'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

if (Test-Path $SoakDir) { Remove-Item $SoakDir -Recurse -Force }
New-Item -ItemType Directory -Force -Path $SoakDir | Out-Null

$stuck = @()
$slowest = 0

foreach ($round in 1..$Count) {
    # A fresh variable store each time, so the firmware makes the same choices
    # on every boot and a failure cannot be the boot entry from the last one.
    $Vars = Join-Path $BuildDir 'vars-soak.fd'
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $Vars -Force
    $Log = Join-Path $SoakDir ("boot-{0:d3}.log" -f $round)

    $QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $Vars -SerialLog $Log `
        -Processors $Processors -Headless

    $process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow
    $reached = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Marker)) { $reached = $true; break }
        }
    }
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
        $process.WaitForExit(5000) | Out-Null
    }

    if ($reached) {
        if ($waited -gt $slowest) { $slowest = $waited }
        Write-Host ("    boot {0,3} reached it in {1}s" -f $round, $waited) -ForegroundColor DarkGray
        if (-not $Keep) { Remove-Item $Log -Force }
    } else {
        # The last line is the whole point: it says where the machine stopped.
        $last = ''
        if (Test-Path $Log) {
            $lines = ((Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '') -split "`r?`n" |
                Where-Object { $_ -ne '' }
            if ($lines.Count -gt 0) { $last = $lines[-1] }
        }
        Write-Host ("    boot {0,3} STUCK after {1}s" -f $round, $waited) -ForegroundColor Red
        Write-Host "        last line: $last" -ForegroundColor Red
        $stuck += [pscustomobject]@{ Round = $round; Last = $last; Log = $Log }
    }
}

Write-Host ''
if ($stuck.Count -eq 0) {
    Write-Host "$Count boots, all of them finished; the slowest took ${slowest}s." -ForegroundColor Green
    exit 0
}

Write-Host "$($stuck.Count) of $Count boots did not finish:" -ForegroundColor Red
foreach ($one in $stuck) {
    Write-Host "    boot $($one.Round): $($one.Last)"
    Write-Host "        $($one.Log)"
}
exit 1
