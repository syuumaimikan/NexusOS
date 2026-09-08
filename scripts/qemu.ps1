<#
.SYNOPSIS
    The machine NexusOS is booted on, in one place.

.DESCRIPTION
    Dot-source this from a script that boots the system:

        . (Join-Path $PSScriptRoot 'qemu.ps1')

    Six scripts started QEMU with six copies of nearly the same argument list,
    and they had already drifted -- one enabled a CPU feature the others did
    not. Adding a device meant editing all of them and hoping. The machine a
    test boots and the machine a screenshot boots have to be the same machine,
    or a test proves something about a configuration nobody runs.
#>

Set-StrictMode -Version Latest

# The disk NexusOS drives, made by make-disk.ps1. Data only: it carries a GPT
# and a FAT32 filesystem, and deliberately nothing to boot from.
function Get-NexusDiskImage {
    param([Parameter(Mandatory = $true)][string]$BuildDir)
    return (Join-Path $BuildDir 'nexus-disk.img')
}

# The image the firmware boots from, when a run is about that.
#
# A separate file from the data disk, and the separation is the point. Both hold
# an EFI system partition, so a machine given both leaves the firmware to choose
# -- and it chose the one whose kernel was older, which made every
# fault-injection build boot a binary that was not the one under test. One disk
# to boot from, one to read, never both.
function Get-NexusBootImage {
    param([Parameter(Mandatory = $true)][string]$BuildDir)
    return (Join-Path $BuildDir 'nexus-boot.img')
}

# Build the argument list.
#
# The firmware variable store is per-caller because a run that changes it must
# not change what the next one boots; everything else is the machine.
function Get-NexusQemuArgs {
    param(
        [Parameter(Mandatory = $true)][string]$BuildDir,
        [Parameter(Mandatory = $true)][string]$EspDir,
        [Parameter(Mandatory = $true)][string]$FirmwareCode,
        [Parameter(Mandatory = $true)][string]$FirmwareVars,
        [Parameter(Mandatory = $true)][string]$SerialLog,
        [int]$Processors = 4,
        [string]$Memory = '1G',
        [int]$MonitorPort = 0,
        [switch]$Headless,
        [switch]$StopOnFault,
        [switch]$BootFromImage
    )

    $arguments = @(
        '-machine', 'q35',
        # `+pdpe1gb` because the bootloader maps physical memory with gigabyte
        # pages when the processor has them, and a machine without the feature
        # exercises a different path.
        '-cpu', 'qemu64,+pdpe1gb',
        '-smp', "$Processors",
        '-m', $Memory,

        # UEFI firmware: read-only code plus a writable variable store.
        '-drive', "if=pflash,format=raw,unit=0,readonly=on,file=$FirmwareCode",
        '-drive', "if=pflash,format=raw,unit=1,file=$FirmwareVars",

        # Serial is the kernel console.
        '-serial', "file:$SerialLog",

        # A triple fault should stop the machine with a diagnosable state rather
        # than silently rebooting into another boot attempt.
        '-no-reboot'
    )

    if ($BootFromImage) {
        # Nothing but the image: the firmware has to find the partition table,
        # the filesystem and the bootloader for itself.
        $boot = Get-NexusBootImage -BuildDir $BuildDir
        if (-not (Test-Path $boot)) { throw "no boot image at $boot" }
        $arguments += @(
            '-drive', "if=none,id=nexusboot,format=raw,file=$boot",
            '-device', 'virtio-blk-pci,drive=nexusboot,disable-modern=on'
        )
    } else {
        # Present build/esp as a FAT filesystem. QEMU's virtual FAT layer means
        # there is no image to rebuild between runs: edit, build, boot.
        $arguments += @('-drive', "format=raw,file=fat:rw:$EspDir")

        # And the disk the kernel drives itself, over virtio. Legacy virtio on
        # purpose: the driver reaches it through an I/O port window rather than
        # the modern transport's memory-mapped capability structures, which is
        # a great deal less code for a first block driver.
        $disk = Get-NexusDiskImage -BuildDir $BuildDir
        if (Test-Path $disk) {
            $arguments += @(
                '-drive', "if=none,id=nexusdisk,format=raw,file=$disk",
                '-device', 'virtio-blk-pci,drive=nexusdisk,disable-modern=on'
            )
        }
    }

    if ($MonitorPort -gt 0) {
        $arguments += @('-monitor', "tcp:127.0.0.1:$MonitorPort,server,nowait")
    }
    if ($Headless) {
        $arguments += @('-display', 'none')
    }
    if ($StopOnFault) {
        $arguments += @('-no-shutdown')
    }

    return $arguments
}
