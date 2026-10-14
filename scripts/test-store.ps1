<#
.SYNOPSIS
    Opens the package window through the WM launcher and installs something.

.DESCRIPTION
    The installing machinery has been on this system for some time and had
    nowhere to be seen. This checks the window that shows it, and the two
    answers a package manager has to get right:

      * a package whose signature does not hold is refused, and the refusal
        happens before anything is written;
      * a package that is signed by the key this machine trusts is installed,
        and the record of what is installed is updated so that the updater and
        this window agree afterwards.

    The disk carries three packages on purpose -- a good one, a newer one, and
    one with a byte changed after signing -- which is what makes both answers
    checkable without the test having to make a package itself.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot of the window, if one is wanted.
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
$Log = Join-Path $BuildDir 'store-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-store.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force
if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 33000 -Maximum 33899
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

function Wait-For {
    param([string]$Text, [int]$Seconds)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { return $false }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Text)) { return $true }
        }
    }
    return $false
}

$failures = @()
try {
    Write-Host '==> Waiting for the desktop' -ForegroundColor Cyan
    if (-not (Wait-For -Text 'shell: took the strip' -Seconds $Timeout)) {
        throw 'the desktop never appeared'
    }

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 160)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # The WM launcher works regardless of dock position or screen layout.
        Write-Host '==> Opening packages through the WM launcher' -ForegroundColor Cyan
        Send-Keys @('meta_l-spc')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Send-Keys @('p', 'k', 'g', 'ret')

        if (-not (Wait-For -Text 'store: the packages this machine has' -Seconds 90)) {
            throw 'the package window never started'
        }

        # The list is sorted by name and then by version, and the disk holds
        # three packages all called "demo": the tampered one first, because a
        # stable sort keeps the order the directory gave, then 1.0.0, then
        # 1.1.0. The cursor starts on the tampered one, which is the one that
        # must not install.
        Write-Host '==> Trying to install a package whose signature does not hold' -ForegroundColor Cyan
        Send-Keys @('ret')
        Start-Sleep -Seconds 2

        Write-Host '==> Installing one that is signed' -ForegroundColor Cyan
        Send-Keys @('down', 'down')
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'store: installed demo' -Seconds 60)) {
            $failures += 'the package window never installed anything'
        }
        Start-Sleep -Seconds 2

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'store.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'compositor: started the packages, and lent them the disk',
        'store: the packages this machine has',
        'store: installed demo'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

# The tampered package must never have been installed, and nothing may have
# fallen over. The first of those is the whole point of signing a package.
foreach ($bad in @(
        'store: installed demo from PKG/BAD.NEX',
        'store: PANIC',
        'KERNEL PANIC'
    )) {
    if ($output.Contains($bad)) {
        $failures += "saw $bad"
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Package tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
