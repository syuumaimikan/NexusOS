<#
.SYNOPSIS
    Writes the image behind the USB stick QEMU plugs into the machine.

.DESCRIPTION
    One mebibyte of sectors, each of which says its own number:

        NEXUS USB SECTOR 000005 NEXUS USB SECTOR 000005 ...

    That is the whole point of it. A block driver that returns the *first*
    sector for every request, or that is out by one, or that reads the right
    number of bytes from the wrong place, passes a test against a disk of zeros
    and fails against this one. The kernel reads block five and checks the text
    says five.

    A separate file from `nexus-disk.img`, which is the disk the kernel already
    drives over virtio. Two different claims -- "this machine can read its own
    disk" and "this machine can read a USB stick" -- and one image would let the
    second be mistaken for the first.

    No filesystem on it. What is being tested is the four layers between the
    controller and the sector; putting a filesystem on top would test the
    filesystem.

.PARAMETER OutputFile
    Where to write it.

.PARAMETER Sectors
    How many 512-byte sectors. The default is one mebibyte's worth.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputFile,
    [int]$Sectors = 2048
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$SectorSize = 512
$bytes = New-Object byte[] ($Sectors * $SectorSize)

for ($sector = 0; $sector -lt $Sectors; $sector++) {
    $text = 'NEXUS USB SECTOR {0:D6} ' -f $sector
    $unit = [System.Text.Encoding]::ASCII.GetBytes($text)
    $at = $sector * $SectorSize
    # Repeated to fill the sector, so that a read of any offset inside it lands
    # on the sector's own number rather than on padding.
    for ($offset = 0; $offset -lt $SectorSize; $offset++) {
        $bytes[$at + $offset] = $unit[$offset % $unit.Length]
    }
}

$directory = Split-Path -Parent $OutputFile
if ($directory -and -not (Test-Path $directory)) {
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
}
[System.IO.File]::WriteAllBytes($OutputFile, $bytes)

Write-Host ("wrote {0}: {1} sectors of {2} bytes" -f $OutputFile, $Sectors, $SectorSize)
