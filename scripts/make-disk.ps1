<#
.SYNOPSIS
    Builds the raw disk image NexusOS drives over virtio.

.DESCRIPTION
    Every sector begins with its own number, written as text. That is the whole
    design, and it is deliberate: a block driver's first job is to fetch the
    sector it was asked for, and the failure it has to be caught making is
    fetching a *different* one. A disk full of zeroes cannot tell those apart,
    and neither can a disk full of random bytes.

    This is not a filesystem. It is the thing a driver is tested against before
    there is a filesystem to confuse the question, and it is replaced by a real
    GPT and FAT32 image once there is a reader for one.

.PARAMETER Output
    Where to write the image.

.PARAMETER SizeMiB
    How large to make it.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Output,
    [int]$SizeMiB = 4
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$SectorSize = 512
$sectors = $SizeMiB * 1024 * 1024 / $SectorSize

$image = New-Object byte[] ($sectors * $SectorSize)

for ($sector = 0; $sector -lt $sectors; $sector++) {
    # Padded to a fixed width so that the text at the start of a sector is the
    # same length whichever sector it is, and a driver that returned a
    # neighbouring one is caught by the number rather than by the layout.
    $label = 'NEXUSOS-SECTOR-{0:D8}' -f $sector
    $bytes = [System.Text.Encoding]::ASCII.GetBytes($label)
    [Array]::Copy($bytes, 0, $image, $sector * $SectorSize, $bytes.Length)
}

[System.IO.File]::WriteAllBytes($Output, $image)

Write-Host "  disk       : $SizeMiB MiB, $sectors sectors  -> $(Split-Path -Leaf $Output)"
