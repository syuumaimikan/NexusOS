<#
.SYNOPSIS
    Answers the first-run wizard on the build's disk, so the tests that come
    after it boot to a desktop.

.DESCRIPTION
    A machine that has never been set up shows the wizard and waits for a
    person. That is the right behaviour and it is what `build.ps1` produces: a
    fresh disk is a machine nobody has configured yet.

    Every other test assumes a machine somebody has already configured -- a
    desktop, a strip, windows. So this is the person: it boots the disk once and
    types the answers through QEMU's keyboard, the same path a real keystroke
    takes, and leaves the disk set up.

    It is not a substitute for `test-setup.ps1`, which is where the wizard is
    actually tested. This only has to get past it, and says so if it cannot.

.PARAMETER Timeout
    How long to wait for the wizard, in seconds.
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
$Log = Join-Path $BuildDir 'configure.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-configure.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 28000 -Maximum 30000
$arguments = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

Write-Host '==> Setting the machine up, so the tests have one to run on' -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $arguments -PassThru -NoNewWindow

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

try {
    if (-not (Wait-For -Text 'setup: this machine has not been set up' -Seconds $Timeout)) {
        # Already configured, or it never got that far. The first is ordinary --
        # this runs against whatever disk is there -- and the second is caught
        # by whatever test comes next, loudly.
        $sofar = if (Test-Path $Log) { (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '' } else { '' }
        if ($sofar.Contains('machine already set up')) {
            Write-Host '    already set up; nothing to answer' -ForegroundColor DarkGray
            exit 0
        }
        throw 'the wizard never appeared, and the machine is not set up either'
    }

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        # Spaced out, because what is being exercised is that the path works
        # rather than how fast it is.
        foreach ($key in @(
                'ret',                                             # language
                'ret',                                             # timezone
                'n', 'e', 'x', 'u', 's', 'ret',                    # a name
                'p', 'a', 's', 's', 'w', 'o', 'r', 'd', 'ret',     # a password
                'p', 'a', 's', 's', 'w', 'o', 'r', 'd', 'ret',     # and again
                'ret'                                              # the network
            )) {
            $writer.WriteLine("sendkey $key")
            Start-Sleep -Milliseconds 220
        }

        if (-not (Wait-For -Text 'setup: wrote settings' -Seconds 90)) {
            throw 'the wizard never wrote the settings'
        }

        # And then wait for the machine to stop writing to that same directory.
        #
        # The wizard is not the only thing that writes to `system/` on a first
        # boot: the installer and the updater both run behind it and both leave
        # a file there. Stopping the machine five seconds after the wizard is
        # done lands in the middle of that, and a directory that was being grown
        # when the power went off is a directory that can come back without the
        # entry the wizard had just put in it -- which shows up two tests later
        # as a machine that says it has never been set up.
        if (-not (Wait-For -Text 'the machine is up to date' -Seconds 120)) {
            if (-not (Wait-For -Text 'update: installed' -Seconds 30)) {
                Write-Host '    the updater never finished; carrying on' -ForegroundColor DarkYellow
            }
        }
        # And let the filesystem finish with all of it before the machine stops.
        Start-Sleep -Seconds 5
        $writer.WriteLine('quit')
        Start-Sleep -Milliseconds 500
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

Write-Host '    the machine is set up' -ForegroundColor DarkGray
