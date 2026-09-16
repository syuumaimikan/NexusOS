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

# How big the disk NexusOS drives is, and how much of it is FAT32.
#
# Here rather than in `build.ps1` for the reason this whole file exists: four
# test scripts build their own fresh copy of that disk, and when the size lived
# in `build.ps1` alone they went on making the old sixty-four megabyte one. The
# machine then booted with a disk a different size from the one the build had
# made, which showed up as `disk 131072 sectors` in a log that should have said
# 16777216 -- and as two tests that could not find a desktop, because the
# configuration had been on the image that got replaced.
function Get-NexusDiskSizes {
    # Eight gigabytes, and the binding constraint is the host rather than
    # anything here.
    #
    # It was raised to thirty-two, because the machine can now unpack software
    # that arrived over the network. That does not fit: **six** scripts build a
    # disk of this size for themselves, and several of their images coexist, so
    # the number here is multiplied by about six on a volume with seventy-two
    # gigabytes free. `make-disk.ps1` ran out of room part-way, `build.ps1`
    # stopped with an error, and what was left behind was a *truncated* image --
    # a 1.1 GB file whose partition table promises sixty-seven million sectors.
    #
    # A machine given that boots, reads its GPT, starts making a filesystem and
    # stops for ever at `begin NexusFS`, which looks exactly like a kernel that
    # hangs.
    #
    # `make-disk.ps1` did say so -- it compares what it wrote against what it
    # meant to write and throws. The error was filtered out of view by the
    # person reading the build's output, who had piped it through a match for
    # the success line. Three investigations followed, of a machine that was
    # behaving exactly as a machine with a truncated disk should.
    #
    # Eight gigabytes leaves 7.9 for NexusFS, which is a great deal of room for
    # anything a browser is going to download.
    return @{ SizeMiB = 8192; FatMiB = 256 }
}

# The image behind the USB stick.
#
# A different file from the disk above on purpose: "the machine can read its
# own disk" and "the machine can read a USB stick" are different claims, and
# one image would let the second be mistaken for the first.
function Get-NexusUsbImage {
    param([Parameter(Mandatory = $true)][string]$BuildDir)
    return (Join-Path $BuildDir 'nexus-usb.img')
}

# And the one with a filesystem on it.
function Get-NexusUsbFsImage {
    param([Parameter(Mandatory = $true)][string]$BuildDir)
    return (Join-Path $BuildDir 'nexus-usb-fs.img')
}

# Refuse a data disk the firmware could boot from.
#
# The rule above is easy to state and easy to break: `make-disk.ps1 -SourceDir`
# copies the EFI system partition tree into the image, and an image with
# `\EFI\BOOT\BOOTX64.EFI` in it is one the firmware will happily start. When
# that happens nothing announces it -- the machine boots, the log looks
# ordinary, and every test is quietly running a kernel staged at image-build
# time instead of the one just compiled. It has cost two debugging sessions.
#
# So it is checked rather than remembered. The short name FAT32 stores is the
# eleven bytes `BOOTX64 EFI`, and finding them anywhere in the image means a
# directory entry names that file.
#
# Read in pieces, and only the first half-gigabyte of them. Both halves of that
# matter now the disk is eight gigabytes: `ReadAllBytes` cannot return an array
# that large at all, and reading the whole of it on every one of the twenty-odd
# scripts that boot the machine would cost more than the boot does. The bound is
# not a guess -- a directory entry can only be inside the FAT32 partition, which
# begins one megabyte in and is a quarter-gigabyte long, so half a gigabyte
# covers all of it and the table in front of it.
# Open an image for reading, waiting out whoever else has it.
#
# A file this size is held by things that are not bugs. A QEMU that has been
# told to quit keeps its handle for a moment while it exits, and a virus scanner
# works through thirty-two freshly written gigabytes on its own schedule. Both
# produce a sharing violation that is over within seconds -- and treating one as
# fatal made a healthy machine look broken three separate times in one session:
# once as "no serial output at all", once as a boot that stopped at
# `begin NexusFS`, and once as a desktop that never appeared.
#
# So it waits, and it says so if it is still waiting after a while, and it only
# gives up when it is clear the holder is not letting go.
function Open-ImageForReading {
    param(
        [Parameter(Mandatory = $true)][string]$Image,
        [int]$Seconds = 90
    )

    $deadline = (Get-Date).AddSeconds($Seconds)
    $said = $false
    while ($true) {
        try {
            return [System.IO.File]::OpenRead($Image)
        } catch [System.IO.IOException] {
            if ((Get-Date) -ge $deadline) {
                throw ("$(Split-Path -Leaf $Image) is still held by another process after " +
                    "$Seconds seconds. A QEMU that did not exit, or something on the host " +
                    'reading it. Nothing can be checked while it is open elsewhere.')
            }
            if (-not $said) {
                Write-Host "    waiting for $(Split-Path -Leaf $Image) to be free" -ForegroundColor DarkGray
                $said = $true
            }
            Start-Sleep -Milliseconds 500
        }
    }
}

function Assert-NotBootable {
    param([Parameter(Mandatory = $true)][string]$Image)

    $needle = [System.Text.Encoding]::ASCII.GetBytes('BOOTX64 EFI')
    $Scan = 512MB
    $ChunkSize = 8MB

    $stream = Open-ImageForReading -Image $Image
    try {
        # One chunk plus the needle's length less one, so that a name lying
        # across a chunk boundary is still whole in the buffer. Without the
        # overlap the check would pass for a file that happened to land there,
        # which is the kind of hole that only shows up once.
        $overlap = $needle.Length - 1
        $buffer = New-Object byte[] ($ChunkSize + $overlap)
        $held = 0
        $read = 0L
        while ($read -lt $Scan) {
            $want = [int][math]::Min([long]$ChunkSize, $Scan - $read)
            $got = $stream.Read($buffer, $held, $want)
            if ($got -le 0) { break }
            $read += $got
            $usable = $held + $got

            # `Array.IndexOf` for the first byte, and the shell's own loop only
            # at the handful of places it lands. Comparing every byte in
            # PowerShell instead took eighty seconds on an eight-gigabyte image
            # and ten on a sixty-four megabyte one -- which every script that
            # boots the machine was paying, every time.
            $last = $usable - $needle.Length
            $index = 0
            while ($index -le $last) {
                $index = [Array]::IndexOf($buffer, $needle[0], $index, $last - $index + 1)
                if ($index -lt 0) { break }
                $found = $true
                for ($offset = 1; $offset -lt $needle.Length; $offset++) {
                    if ($buffer[$index + $offset] -ne $needle[$offset]) { $found = $false; break }
                }
                if ($found) {
                    throw ("$(Split-Path -Leaf $Image) contains EFI\BOOT\BOOTX64.EFI, so the " +
                        'firmware would have two disks to choose between and could boot the ' +
                        'wrong kernel. Build it with -ProgramDir, not -SourceDir.')
                }
                $index++
            }

            # Carry the tail forward for the next pass to look at again.
            $held = [math]::Min($overlap, $usable)
            [Array]::Copy($buffer, $usable - $held, $buffer, 0, $held)
        }
    } finally {
        $stream.Close()
    }
}

# Refuse a data disk that is not the size the build makes.
#
# Checked rather than remembered, for the same reason as the rule above it. A
# script that built its own copy of this disk and forgot the size produced an
# image the machine booted perfectly well -- and the configuration, the store
# and everything else a previous run had put there were on the image that had
# just been replaced. It reads as "the desktop never appeared", two tests later,
# in a script that did nothing wrong.
function Assert-DiskSize {
    param([Parameter(Mandatory = $true)][string]$Image)

    $want = [long](Get-NexusDiskSizes).SizeMiB * 1MB
    $got = (Get-Item $Image).Length
    if ($got -ne $want) {
        throw ("$(Split-Path -Leaf $Image) is $([math]::Round($got / 1MB)) MiB and the build " +
            "makes $([math]::Round($want / 1MB)) MiB. Something rebuilt it without asking " +
            'Get-NexusDiskSizes; run build.ps1 to make it again.')
    }
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
        # Which host port reaches the guest's TCP service. Different per run, so
        # that two tests in flight at once do not fight over one socket -- and
        # so that a stale QEMU holding the port is a test that says so rather
        # than one that quietly talks to the wrong machine.
        [int]$HostHttpPort = 18080,
        [switch]$Headless,
        [switch]$StopOnFault,
        [switch]$BootFromImage
    )

    $arguments = @(
        '-machine', 'q35',
        # `+pdpe1gb` because the bootloader maps physical memory with gigabyte
        # pages when the processor has them, and a machine without the feature
        # exercises a different path.
        '-cpu', 'qemu64,+pdpe1gb,+rdrand,+rdseed',
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
            Assert-NotBootable -Image $disk
            Assert-DiskSize -Image $disk
            $arguments += @(
                '-drive', "if=none,id=nexusdisk,format=raw,file=$disk",
                '-device', 'virtio-blk-pci,drive=nexusdisk,disable-modern=on'
            )
        }
    }

    # A network card, on the same legacy virtio transport as the disk and for
    # the same reason: an I/O port window instead of the modern transport's
    # memory-mapped capability structures.
    #
    # QEMU's user-mode networking rather than a tap: it needs no privileges and
    # no host configuration, and it brings a DHCP server at 10.0.2.2, a DNS
    # forwarder at 10.0.2.3 and a gateway that answers ICMP. Everything the
    # guest does on it is real -- real frames, real ARP, a real lease -- and
    # none of it needs the host to be set up first.
    #
    # And a way in. `hostfwd` makes QEMU listen on the host and forward what
    # arrives to a port in the guest, which is what lets a test open a real TCP
    # connection to the kernel's own listener -- a real handshake, real sequence
    # numbers, and a client that has no idea what it is talking to. Bound to
    # the loopback address on purpose: this exposes a service written this month
    # and it has no business being reachable from anywhere else.
    $arguments += @(
        '-netdev', "user,id=nexusnet,hostfwd=tcp:127.0.0.1:${HostHttpPort}-:80",
        '-device', 'virtio-net-pci,netdev=nexusnet,disable-modern=on'
    )

    # The GPU, and it is the display.
    #
    # `-vga none` removes the firmware's. That sounds like it would leave two
    # drivers on one device -- the firmware's and the kernel's -- and it is the
    # opposite: this firmware has no driver for a virtio GPU at all, so a
    # machine given one and nothing else is handed *no framebuffer*, the
    # bootloader says so, and the kernel's own driver becomes the only thing
    # that can put anything on the screen. There is no handover and no moment
    # when both are driving it.
    #
    # Named, so a screenshot can be asked for of this display by name rather
    # than of whichever one QEMU counts as first.
    #
    # Still beside the firmware's display and not instead of it. `-vga none`
    # works -- the desktop comes up on the GPU, the compositor's damage
    # rectangles reach it, 59 of them in a session instead of a full screen
    # twenty times a second -- and it is not here, because it takes the disk's
    # interrupt vector from forty thousand entries in a session to twenty-seven
    # million. See docs/gpu.md; the machinery is all in the kernel and this one
    # line is what turns it on, once that is understood.
    # The size is given, not taken. A virtio GPU's default scanout is 1280x800
    # and the firmware's VGA was 1920x1200, so adopting the GPU as the display
    # silently made everybody's screen smaller -- which showed up as a wallpaper
    # reporting `1280x764` where every test that knew the machine expected
    # `1920x1164`, and as three input tests clicking where a window no longer
    # was. The display is part of what this machine *is*, so it is stated here
    # with the rest of it.
    $arguments += @(
        '-vga', 'none',
        '-device', 'virtio-gpu-pci,id=nexusgpu,xres=1920,yres=1200'
    )

    # A sound card, and nothing to play it through.
    #
    # `-audiodev none` is deliberate and is the whole reason this can be tested
    # at all: the card is real to the guest -- it enumerates on the PCI bus, its
    # codec answers, and its DMA engine reads the guest's memory -- and the
    # samples go nowhere on the host. A test machine that opened the developer's
    # speakers every time it booted would be a test nobody runs twice.
    #
    # AC'97 rather than Intel HD Audio because it is the simpler of the two by a
    # long way: a descriptor list and a codec on a serial link, against HDA's
    # command ring, response ring, widget graph and stream descriptors. The
    # roadmap said "AC'97 or Intel HD Audio" and this is the one that is a
    # driver rather than a project.
    $arguments += @(
        '-audiodev', 'none,id=nexusquiet',
        '-device', 'AC97,audiodev=nexusquiet'
    )

    # An xHCI controller, and a USB drive plugged into it.
    #
    # xHCI rather than the older UHCI or EHCI because it is what a machine built
    # this decade actually has, and because the older ones are a different
    # driver rather than a simpler version of this one. `qemu-xhci` is the
    # standards-compliant model; `nec-usb-xhci` is a particular vendor's.
    #
    # The drive is a separate image from the one the kernel already drives, so
    # that "the machine can read its own disk" and "the machine can read a USB
    # stick" cannot be confused for one another.
    $stick = Get-NexusUsbImage -BuildDir $BuildDir
    $formatted = Get-NexusUsbFsImage -BuildDir $BuildDir
    if ((Test-Path $stick) -or (Test-Path $formatted)) {
        $arguments += @('-device', 'qemu-xhci,id=nexusxhci')
    }
    if (Test-Path $stick) {
        $arguments += @(
            '-drive', "if=none,id=nexusstick,format=raw,file=$stick",
            '-device', 'usb-storage,bus=nexusxhci.0,drive=nexusstick'
        )
    }
    # A second stick, with a filesystem on it. Two rather than one because the
    # two prove different things: the raw one proves blocks reach the right
    # place, and this one proves a filesystem can be read off a drive that
    # arrives over four layers of USB. Having both also means the driver has to
    # cope with more than one device, which is a thing it could quietly not do.
    if (Test-Path $formatted) {
        $arguments += @(
            '-drive', "if=none,id=nexusformatted,format=raw,file=$formatted",
            '-device', 'usb-storage,bus=nexusxhci.0,drive=nexusformatted'
        )
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
