<#
.SYNOPSIS
    Why a fresh NexusFS format takes half a minute, measured one variable at a
    time.

.DESCRIPTION
    A fresh format of the same 8 GiB disk had been measured at 709 ms once and
    at 32-37 seconds eight times. Per transfer that is 21.6 microseconds against
    996-1122 -- and the kernel's timer runs at 1 kHz, so the slow figure looked
    like exactly one tick, every time.

    It was not a tick. This script is what found that out, and the order the
    modes are in is the order the wrong answers were eliminated:

      -Repeat N            the same machine N times, which had to come first.
                           Identical runs measured 34242, 32807, 32169 and
                           17610 ms -- a 1.9x spread. Without that number the
                           comparisons below are unreadable, and the first pass
                           of this script did put two figures from inside that
                           spread beside each other and call the difference a
                           result.
      -WithoutNetwork      the card shares the disk's interrupt line, and the
                           one fast log did not. Removing it unshared the line
                           and the format got *slower*. Not the cause.
      -NoVga               the fast log had no display device either. Removing
                           it did not finish a format in 400 seconds. Not the
                           cause, and in the opposite direction.
      -Whpx                nothing here has ever passed `-accel`, so this had
                           been running on TCG. Hardware acceleration changed
                           nothing: 34739 and 29491 ms. Not the cause.
      -Cache <mode>        `cache=unsafe` -- flushes ignored -- formats in 2294
                           and 2724 ms. Thirteen times faster, and the spread
                           collapses from 1.9x to 1.2x.

    So the time is flushes, and the reason is in this repository rather than on
    the host: `virtio_blk.rs` writes **zero** to the device's feature register,
    which refuses `VIRTIO_BLK_F_FLUSH` although the device offers it
    (`features 0x71007ed4`, bit 9 set). QEMU will not give a writeback cache to
    a guest that cannot flush, so it runs the drive write-through and every
    single write waits for the host's disk. On NTFS that is about a millisecond,
    which is the number this whole investigation kept landing on.

    `cache=unsafe` is not a fix and must not be shipped: a host that loses power
    loses the filesystem, and the journal's crash tests would be testing a
    machine that cannot crash. It is here because it identifies the cost in one
    boot.

    Nothing here touches build/nexus-disk.img. Each run makes its own image,
    because a format can only be measured on a disk that has not been formatted.
#>
[CmdletBinding()]
param([int]$Timeout = 400, [int]$Repeat = 0, [switch]$NoVga, [switch]$Whpx, [string]$Cache = '', [switch]$Mount, [switch]$LateDisplay)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$ProgramDir = Join-Path $BuildDir 'programs'
$ExpDisk = Join-Path $BuildDir 'exp-disk.img'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}

$sizes = Get-NexusDiskSizes

function New-FreshDisk {
    if (Test-Path $ExpDisk) { Remove-Item $ExpDisk -Force }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass `
        -File (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $ExpDisk `
        -ProgramDir $ProgramDir -SizeMiB $sizes.SizeMiB -FatMiB $sizes.FatMiB | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'could not make the experiment disk' }
}

# One boot, to the point where the filesystem has been made, and no further.
# The second boot, on the disk the first one made.
#
# `Measure-Format` makes a fresh image every time, which is right for measuring
# a format and useless for measuring a mount -- a mount reads a filesystem that
# has to already be there. Without this the only mount figures available came
# from whichever test happened to run last, on a disk carrying whatever the
# tests before it had put on it, and comparing two of those says nothing. It
# was compared anyway, once, and read as a four-fold regression.
function Measure-Mount {
    param([string]$Name)
    $first = Measure-Format -Name "$Name-format"
    if (-not $first.Formatted) { throw "$Name never made a filesystem to mount" }
    # And again, on the same image, with no `New-FreshDisk` in between.
    Measure-Format -Name "$Name-mount" -KeepDisk
}

function Measure-Format {
    param([string]$Name, [switch]$WithoutNetwork, [switch]$WithoutDisplay, [switch]$Accelerated, [string]$CacheMode = '', [switch]$KeepDisk, [switch]$DisplayLast)

    if (-not $KeepDisk) { New-FreshDisk }
    $log = Join-Path $BuildDir "irq-$Name.log"
    if (Test-Path $log) { Remove-Item $log -Force }
    $vars = Join-Path $BuildDir "vars-irq-$Name.fd"
    Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $vars -Force

    $args = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
        -FirmwareCode $FirmwareCode -FirmwareVars $vars -SerialLog $log `
        -MonitorPort (Get-Random -Minimum 43000 -Maximum 43999) -Headless

    # The experiment's own disk, in place of the one every other script shares.
    $args = $args | ForEach-Object { $_ -replace [regex]::Escape((Get-NexusDiskImage -BuildDir $BuildDir)), $ExpDisk }

    if ($WithoutNetwork) {
        # Drop the two arguments that make the card, and the flag in front of
        # each. Nothing else moves -- the display, the GPU, the sound card and
        # the USB controller all stay in the slots they were in.
        $kept = @()
        for ($i = 0; $i -lt $args.Count; $i++) {
            if ($args[$i] -eq '-netdev' -and $args[$i + 1] -like 'user,id=nexusnet*') { $i++; continue }
            if ($args[$i] -eq '-device' -and $args[$i + 1] -like 'virtio-net-pci*') { $i++; continue }
            $kept += $args[$i]
        }
        $args = $kept
    }

    if ($CacheMode) {
        # How the host treats what the guest writes. The default is
        # `writeback`, under which a flush from the guest becomes an fsync on an
        # eight-gigabyte file -- and NTFS answers one of those in about a
        # millisecond, which is the number this whole investigation keeps
        # landing on. `unsafe` ignores flushes entirely: it is not a setting to
        # ship, because a host that loses power loses the filesystem, but it
        # says in one boot whether flushes are what the time is going into.
        $args = $args | ForEach-Object { $_ -replace 'if=none,id=nexusdisk,format=raw', "if=none,id=nexusdisk,format=raw,cache=$CacheMode" }
    }

    if ($Accelerated) {
        # Windows Hypervisor Platform, which this QEMU supports and which
        # nothing in this repository has ever asked for. Without `-accel` the
        # binary picks its built-in default, and on a Windows build that is
        # `tcg` -- every guest instruction translated in software. That is a
        # whole-machine slowdown, not a disk one, which is why it survived a
        # day of looking at the disk.
        $args = @('-accel', 'whpx,kernel-irqchip=off') + $args
    }

    if ($DisplayLast) {
        # A display, but not at the front of the bus. `-vga none` stops QEMU
        # putting its own VGA in the first slot, and a `-device VGA` at the end
        # of the line lands after everything else -- so the disk keeps 00:01.0
        # and interrupt line 10, exactly as it has when the firmware gives no
        # framebuffer, while the firmware still gets one.
        #
        # That separates the two things `-vga none` changes at once. Without it
        # the slow headless runs and the one fast headless log differ in the
        # display *and* in where every PCI device sits, and no comparison
        # between them can say which mattered.
        $args += @('-vga', 'none', '-device', 'VGA')
    }

    if ($WithoutDisplay) {
        # The one thing left that the 709 ms run differed by. The firmware has
        # no driver for the virtio GPU, so a machine with no VGA is handed no
        # framebuffer at all and the kernel runs headless -- which is exactly
        # what that log says it did.
        $args += @('-vga', 'none')
    }

    Write-Host "==> $Name" -ForegroundColor Cyan
    $p = Start-Process -FilePath $QemuExe.Source -ArgumentList $args -PassThru -NoNewWindow
    try {
        for ($waited = 0; $waited -lt $Timeout; $waited++) {
            Start-Sleep -Seconds 1
            if ($p.HasExited) { break }
            if (Test-Path $log) {
                $sofar = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
                if ($sofar -match 'end NexusFS') { break }
            }
        }
    } finally {
        if (-not $p.HasExited) { try { $p.Kill() } catch { } }
        $p.WaitForExit(5000) | Out-Null
    }

    $text = (Get-Content $log -Raw -Encoding UTF8) -replace "`0", ''
    $ms = if ($text -match 'end NexusFS: (\d+) ms') { [int]$Matches[1] } else { -1 }
    $irq = if ($text -match '\[blk \] disk on IRQ (\d+)') { $Matches[1] } else { '?' }
    $slot = if ($text -match 'virtio disk at ([0-9a-f:.]+):') { $Matches[1] } else { '?' }
    $shared = $text -match 'shares IRQ \d+ with the disk'
    $made = $text -match 'NexusFS made at sector'
    [pscustomobject]@{
        Run = $Name; Ms = $ms; Irq = $irq; Slot = $slot; Shared = $shared; Formatted = $made; Log = $log
    }
}

$results = @()
if ($Repeat -gt 0) {
    # The same machine, the same arguments, N times. Without this number no
    # comparison above it means anything: the first pass of this script put
    # 16772 ms and 32030 ms beside each other and called the difference a
    # result, when an earlier run of the *first* configuration had measured
    # 32681 ms. A spread that wide between identical runs is the measurement
    # talking, not the machine.
    for ($n = 1; $n -le $Repeat; $n++) {
        if ($LateDisplay) {
            $results += Measure-Format -Name "irq10-with-display-$n" -DisplayLast
        } elseif ($Mount) {
            $results += Measure-Mount -Name "run$n"
        } elseif ($Cache) {
            $results += Measure-Format -Name "cache-$Cache-$n" -CacheMode $Cache
        } elseif ($Whpx) {
            $results += Measure-Format -Name "whpx-$n" -Accelerated
        } elseif ($NoVga) {
            $results += Measure-Format -Name "novga-$n" -WithoutDisplay
        } else {
            $results += Measure-Format -Name "same-$n"
        }
    }
    Write-Host ''
    $results | Format-Table Run, Ms, Slot, Irq, Shared, Formatted -AutoSize
    $times = @($results | ForEach-Object { $_.Ms })
    $min = ($times | Measure-Object -Minimum).Minimum
    $max = ($times | Measure-Object -Maximum).Maximum
    Write-Host ''
    Write-Host "identical runs: $($times -join ', ') ms" -ForegroundColor Gray
    Write-Host "spread: $min to $max ms ($([math]::Round($max / [math]::Max($min,1), 1))x)" -ForegroundColor Yellow
    exit 0
}
$results += Measure-Format -Name 'with-network'
$results += Measure-Format -Name 'without-network' -WithoutNetwork

Write-Host ''
$results | Format-Table Run, Ms, Slot, Irq, Shared, Formatted -AutoSize

$bad = $results | Where-Object { -not $_.Formatted }
if ($bad) {
    foreach ($b in $bad) { Write-Host "    FAIL $($b.Run) never made a filesystem; see $($b.Log)" -ForegroundColor Red }
    exit 1
}

$with = ($results | Where-Object Run -eq 'with-network').Ms
$without = ($results | Where-Object Run -eq 'without-network').Ms
Write-Host ''
Write-Host "with the card    : $with ms" -ForegroundColor Gray
Write-Host "without the card : $without ms" -ForegroundColor Gray
if ($without -lt ($with / 4)) {
    Write-Host 'The shared interrupt line is the cause.' -ForegroundColor Green
} elseif ($with -lt ($without / 4)) {
    Write-Host 'Removing the card made it SLOWER. Neither hypothesis survives this.' -ForegroundColor Yellow
} else {
    Write-Host 'The shared line is not the cause; the display or the host is.' -ForegroundColor Yellow
}
exit 0
