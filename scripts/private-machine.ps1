<#
.SYNOPSIS
    A copy of the built machine that one test run has to itself, and a host port
    nothing else is listening on.

.DESCRIPTION
    Three agents work in this checkout at once, and `build/` is shared. Two
    things go wrong because of it, and neither says what it is:

    **A build cannot write what a QEMU is holding.** `build/esp` is handed to
    QEMU as `fat:rw:` -- a live view of a host directory, written as well as
    read -- and `build/nexus-disk.img` and the two USB images are opened for
    writing. While a test is running, somebody else's `build.ps1` dies at
    `llvm-objcopy: error: permission denied` writing `build/esp/nexus/kernel.elf`,
    with nothing in the message about QEMU. It has happened in both directions
    here, repeatedly, and has cost hours.

    **Two machines on one disk image do not both work.** The second hangs at
    `begin NexusFS` with no error. A test that shares the image with another
    run is a test whose failures mean nothing.

    So a run takes its own copy. The copy is made once and reused: comparing
    timestamps against the shared image was the obvious rule and is wrong,
    because that file is rewritten by every run anybody makes, so "newer than
    mine" is true every time and the copy is never reused. `-Fresh` after a
    rebuild is the honest way to say what is meant.

    The disk image is eight gigabytes, so this costs eight gigabytes once per
    directory. That is the price of two runs not corrupting each other, and it
    is why every script in one family shares one directory rather than taking
    one each.
#>

Set-StrictMode -Version Latest

<#
.SYNOPSIS
    Copy the built machine into a directory of this family's own, if it is not
    already there, and hand back where it went.

.PARAMETER BuildDir
    The shared `build/`.

.PARAMETER Name
    The directory under `build/` to copy into. Scripts that never run at the
    same time should share one; scripts that might must not.

.PARAMETER Fresh
    Take the copies again even if they are there. What to pass after a rebuild.
#>
function Get-PrivateMachine {
    param(
        [Parameter(Mandatory = $true)][string]$BuildDir,
        [Parameter(Mandatory = $true)][string]$Name,
        [switch]$Fresh
    )

    $private = Join-Path $BuildDir $Name
    if (-not (Test-Path $private)) { New-Item -ItemType Directory $private | Out-Null }

    # The EFI tree first: it is small, and it is the one a build writes into
    # while a machine is running, so it is the one that blocks somebody else.
    $sharedEsp = Join-Path $BuildDir 'esp'
    $privateEsp = Join-Path $private 'esp'
    if (-not (Test-Path (Join-Path $sharedEsp 'EFI\BOOT\BOOTX64.EFI'))) {
        throw 'no staged ESP; run build.ps1 first'
    }
    if ((-not (Test-Path $privateEsp)) -or $Fresh) {
        if (Test-Path $privateEsp) { Remove-Item $privateEsp -Recurse -Force }
        Write-Host '==> Taking this run its own copy of the EFI partition tree' -ForegroundColor Cyan
        Copy-Item $sharedEsp $privateEsp -Recurse -Force
    }

    # Then the images QEMU opens for writing. Named rather than globbed: a glob
    # would sweep up every `.img` anybody has left in `build/`, including the
    # ones another agent's run is holding open, and copying one of those is how
    # this whole class of problem started.
    foreach ($image in 'nexus-disk.img', 'nexus-usb.img', 'nexus-usb-fs.img', 'nexus-boot.img') {
        $from = Join-Path $BuildDir $image
        $to = Join-Path $private $image
        if (-not (Test-Path $from)) { continue }
        if ((Test-Path $to) -and -not $Fresh) { continue }
        Write-Host "==> Copying $image for this run alone" -ForegroundColor Cyan
        # Opened sharing read *and* write, because the shared copy may be open
        # in somebody else's QEMU at this moment. Reading it while they have it
        # is fine; what is not fine is holding it ourselves for the whole run.
        $source = [System.IO.File]::Open(
            $from, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite)
        try {
            $destination = [System.IO.File]::Create($to)
            try { $source.CopyTo($destination) } finally { $destination.Dispose() }
        } finally { $source.Dispose() }
    }

    [PSCustomObject]@{
        BuildDir = $private
        EspDir   = $privateEsp
    }
}

<#
.SYNOPSIS
    A host TCP port nothing is listening on, found by trying to listen on it.

.DESCRIPTION
    Picking at random and hoping is what these scripts used to do, out of the
    same thousand values every other script here picks from. When the pick
    collides QEMU cannot bind its monitor and exits before the machine boots,
    and what is left is a serial log holding nothing but the firmware's
    clear-screen codes -- which reads exactly like a machine that died for no
    reason.

    Still a race: something can take the port between this check and QEMU's
    bind. It turns likely into unlikely, which is the whole of what is claimed.
#>
function Get-FreeMonitorPort {
    param([int]$From = 34000, [int]$To = 39000)
    for ($attempt = 0; $attempt -lt 40; $attempt++) {
        $candidate = Get-Random -Minimum $From -Maximum $To
        try {
            $listener = New-Object System.Net.Sockets.TcpListener(
                [System.Net.IPAddress]::Loopback, $candidate)
            $listener.Start()
            $listener.Stop()
            return $candidate
        } catch {
            # Taken. Try another.
        }
    }
    throw 'no free port for the QEMU monitor'
}
