<#
.SYNOPSIS
    Opens the file manager and copies a file onto a USB drive.

.DESCRIPTION
    The claim is that one window reaches both the machine's own store and a
    removable drive, and can move a file between them. Reading is the easy half;
    the copy is the one that matters, because it goes:

      a file on the store -> read through a directory handle -> written through
      the removable-drive channel -> the kernel's service -> FAT32 cluster
      allocation -> four layers of USB -> the host's image file

    So the strongest check is not in the guest at all: after the run, the file's
    bytes are in `build/nexus-usb-fs.img`, which this script looks for.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.

.PARAMETER Shot
    Where to put a screenshot, if one is wanted.
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
$Log = Join-Path $BuildDir 'files-test.log'
$UsbImage = Join-Path $BuildDir 'nexus-usb-fs.img'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-files.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 37000 -Maximum 37999
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
$checks = 0

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

        Write-Host '==> Opening the file manager' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        # `fil` finds "Files" and nothing else on the list.
        Send-Keys @('f', 'i', 'l')
        Start-Sleep -Seconds 1
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started a file manager' -Seconds 60)) {
            $failures += 'the compositor never started a file manager'
        }
        if (-not (Wait-For -Text 'files: one window for' -Seconds 60)) {
            $failures += 'the file manager never said it was up'
        }
        Start-Sleep -Seconds 2

        # It starts on the store, whose root holds directories and no files --
        # so go into one. `down` moves to `PKG`, `ret` opens it, and the first
        # thing inside is a file, which is what `f2` then copies. Pressing `f2`
        # on the directory itself is refused, correctly, with "this copies
        # files, not folders"; the first version of this test did exactly that
        # and read the refusal as a failure to copy.
        Write-Host '==> Choosing a file and copying it to the drive' -ForegroundColor Cyan
        Send-Keys @('down')
        Start-Sleep -Milliseconds 400
        Send-Keys @('ret')
        Start-Sleep -Seconds 2
        Send-Keys @('f2')
        if (-not (Wait-For -Text 'files: copied' -Seconds 60)) {
            $failures += 'the file manager never copied anything'
        }
        Start-Sleep -Seconds 2

        # And across to the drive, to see it there.
        #
        # The wait is long because switching is not free: the other pane lists a
        # drive, and listing one mounts it afresh and reads the directory over
        # four layers of USB. Two seconds caught the window mid-repaint and the
        # screenshot came out blank -- with every check above still passing,
        # which is exactly the sort of evidence that misleads.
        # `right`, not `tab`: the compositor takes Tab to move the focus between
        # windows and never passes it on, so the file manager could not see it.
        Write-Host '==> Switching to the drive' -ForegroundColor Cyan
        Send-Keys @('right')
        Start-Sleep -Seconds 6

        if ($Shot) {
            $Ppm = Join-Path $BuildDir 'files.ppm'
            Invoke-Screendump -Writer $writer -Path $Ppm
            Convert-PpmToPng -PpmPath $Ppm -PngPath $Shot | Out-Null
            Write-Host "    Screenshot: $Shot" -ForegroundColor DarkGray
        }

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

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'compositor: started a file manager, and lent it the disk and the drives',
        'files: one window for',
        'files: copied',
        # The other pane really was reached, rather than the key going nowhere.
        'files: looking at drive 1'
    )) {
    $checks++
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never said '$expected'"
        Write-Host "    FAIL $expected" -ForegroundColor Red
    }
}

# The check that matters, and it is outside the machine: whatever was copied
# has to be in the host's own image file. A guest that reported a copy and
# wrote nothing passes every check above and fails this one.
$checks++
$name = [regex]::Match($output, 'files: copied (\S+) \((\d+) bytes\)')
if (-not $name.Success) {
    $failures += 'the log never said what was copied'
    Write-Host '    FAIL the log never said what was copied' -ForegroundColor Red
} else {
    $copied = $name.Groups[1].Value
    $bytes = [int]$name.Groups[2].Value
    Write-Host "    .... looking for $copied ($bytes bytes) in the host's image" -ForegroundColor DarkGray
    $image = [System.IO.File]::ReadAllBytes($UsbImage)
    # The name, as FAT32 stores it: eight and three, padded, upper case.
    $stem, $extension = $copied.ToUpperInvariant() -split '\.', 2
    $short = ($stem.PadRight(8) + $extension.PadRight(3)).Substring(0, 11)
    $needle = [System.Text.Encoding]::ASCII.GetBytes($short)
    $found = $false
    for ($at = 0; $at -le $image.Length - $needle.Length; $at++) {
        $match = $true
        for ($index = 0; $index -lt $needle.Length; $index++) {
            if ($image[$at + $index] -ne $needle[$index]) { $match = $false; break }
        }
        if ($match) { $found = $true; break }
    }
    if ($found) {
        Write-Host "    ok   $short is in the host's image at offset $at" -ForegroundColor DarkGray
    } else {
        $failures += "the copied file's directory entry is not in $UsbImage"
        Write-Host "    FAIL $short is not in the host's image" -ForegroundColor Red
    }
}

foreach ($bad in @('KERNEL PANIC', 'files: PANIC', 'files: FAILED')) {
    $checks++
    if ($output.Contains($bad)) {
        $failures += "absent: $bad : it appeared"
        Write-Host "    FAIL absent: $bad" -ForegroundColor Red
    } else {
        Write-Host "    ok   absent: $bad" -ForegroundColor DarkGray
    }
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host "File manager tests passed ($checks checks)." -ForegroundColor Green
    exit 0
} else {
    foreach ($why in $failures) { Write-Host "    FAIL $why" -ForegroundColor Red }
    Write-Host "$($failures.Count) of $checks checks failed. Log: $Log" -ForegroundColor Red
    exit 1
}
