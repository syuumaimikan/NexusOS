<#
.SYNOPSIS
    Builds the disk image NexusOS drives: a GPT with a FAT32 EFI system
    partition, holding the same files the ESP tree does.

.DESCRIPTION
    Until now the system booted from a directory QEMU pretends is a filesystem.
    That is excellent for iteration -- edit, build, boot, with no image to
    rebuild -- and it means NexusOS had never seen a partition table, a boot
    sector, or a file allocation table. Everything about the layout was
    something QEMU was doing on its behalf.

    This writes the real thing: a protective MBR, a GPT with its backup at the
    far end, and inside the partition a FAT32 filesystem built here rather than
    by a formatter. Nothing about it is a stub; a firmware that reads it finds
    what a firmware expects, and so does the kernel.

    Sectors outside the partition are left with their own number written as
    text. That is what the block driver is tested against, and keeping it means
    the driver test does not need a filesystem reader to say whether the driver
    fetched the sector it was asked for.

.PARAMETER Output
    Where to write the image.

.PARAMETER SourceDir
    A directory tree to copy into the partition. Names must fit 8.3.

.PARAMETER ProgramDir
    A directory of programs, copied into `BIN`. Kept separate from the tree the
    firmware boots so that the data disk can carry programs without becoming
    bootable, which is what stops the firmware choosing between two disks.

.PARAMETER SizeMiB
    How large to make the image.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Output,
    [string]$SourceDir,
    [string]$ProgramDir,
    [int]$SizeMiB = 64
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$SectorSize = 512
$TotalSectors = $SizeMiB * 1024 * 1024 / $SectorSize

# The partition starts a megabyte in, which is where every partitioner has put
# the first one for fifteen years: it clears the GPT and aligns the filesystem
# to any erase block a flash device is likely to have.
$PartitionStart = 2048
# The backup GPT needs the last sector for its header and the 32 before it for
# its copy of the entries.
$PartitionEnd = $TotalSectors - 34
$PartitionSectors = $PartitionEnd - $PartitionStart + 1

$image = New-Object byte[] ($TotalSectors * $SectorSize)

# ---------------------------------------------------------------------------
# Sector labels, everywhere. Overwritten by whatever is laid on top.
# ---------------------------------------------------------------------------
for ($sector = 0; $sector -lt $TotalSectors; $sector++) {
    $label = 'NEXUSOS-SECTOR-{0:D8}' -f $sector
    $bytes = [System.Text.Encoding]::ASCII.GetBytes($label)
    [Array]::Copy($bytes, 0, $image, $sector * $SectorSize, $bytes.Length)
}

function Set-U16 { param([int]$Offset, [int]$Value)
    $image[$Offset] = $Value -band 0xFF
    $image[$Offset + 1] = ($Value -shr 8) -band 0xFF
}
function Set-U32 { param([int]$Offset, [uint32]$Value)
    for ($i = 0; $i -lt 4; $i++) { $image[$Offset + $i] = [byte](($Value -shr (8 * $i)) -band 0xFF) }
}
function Set-U64 { param([int]$Offset, [uint64]$Value)
    for ($i = 0; $i -lt 8; $i++) { $image[$Offset + $i] = [byte](($Value -shr (8 * $i)) -band 0xFF) }
}
function Set-Bytes { param([int]$Offset, [byte[]]$Value)
    [Array]::Copy($Value, 0, $image, $Offset, $Value.Length)
}
function Clear-Range { param([int]$Offset, [int]$Length)
    [Array]::Clear($image, $Offset, $Length)
}

# CRC-32, which the GPT uses for its header and its entry array. Written out
# rather than reached for: there is no such thing in the shell, and a table of
# 256 entries is a few lines.
#
# Everything is kept in 64-bit signed arithmetic and masked, because PowerShell
# reads a hexadecimal literal that fits in 32 bits as a *signed* one: `0xFFFFFFFF`
# is minus one, and the polynomial `0xEDB88320` is negative. Casting either to an
# unsigned type fails outright, which is at least a loud way to find out.
$Mask32 = 0xFFFFFFFFL
$crcTable = New-Object long[] 256
for ($i = 0; $i -lt 256; $i++) {
    $c = [long]$i
    for ($k = 0; $k -lt 8; $k++) {
        if ($c -band 1) {
            $c = (0xEDB88320L -bxor ($c -shr 1)) -band $Mask32
        } else {
            $c = ($c -shr 1) -band $Mask32
        }
    }
    $crcTable[$i] = $c
}
function Get-Crc32 { param([byte[]]$Data, [int]$Offset, [int]$Length)
    $crc = 0xFFFFFFFFL
    for ($i = 0; $i -lt $Length; $i++) {
        $index = [int](($crc -bxor $Data[$Offset + $i]) -band 0xFF)
        $crc = ($crcTable[$index] -bxor ($crc -shr 8)) -band $Mask32
    }
    return [uint32](($crc -bxor 0xFFFFFFFFL) -band $Mask32)
}

# ---------------------------------------------------------------------------
# FAT32 inside the partition
# ---------------------------------------------------------------------------
# One sector per cluster. Larger clusters waste less of the FAT and more of the
# disk, and the usual trade does not apply here: FAT32 is only FAT32 above 65525
# clusters -- below that the specification says the volume is FAT16, and
# firmware that checks will refuse it. At four kilobytes a sixty-four megabyte
# image has sixteen thousand clusters and is not a FAT32 volume at all.
$SectorsPerCluster = 1
$ReservedSectors = 32
$FatCount = 2

# The formula from Microsoft's own specification. Solving exactly for the FAT
# size needs the cluster count, which needs the FAT size; this over-estimates by
# at most a sector, which costs nothing and is what every formatter does.
$tmp1 = $PartitionSectors - $ReservedSectors
$tmp2 = (256 * $SectorsPerCluster) + $FatCount
$FatSectors = [int][math]::Floor(($tmp1 + $tmp2 - 1) / $tmp2)

$FatStart = $PartitionStart + $ReservedSectors
$DataStart = $FatStart + $FatCount * $FatSectors
$ClusterCount = [int][math]::Floor(($PartitionEnd - $DataStart + 1) / $SectorsPerCluster)

if ($ClusterCount -lt 65525) {
    throw "only $ClusterCount clusters; FAT32 needs at least 65525"
}

# The boot sector, and its backup at sector six.
$boot = $PartitionStart * $SectorSize
Clear-Range -Offset $boot -Length $SectorSize
Set-Bytes -Offset $boot -Value ([byte[]](0xEB, 0x58, 0x90))
Set-Bytes -Offset ($boot + 3) -Value ([System.Text.Encoding]::ASCII.GetBytes('NEXUSOS '))
Set-U16 -Offset ($boot + 11) -Value $SectorSize
$image[$boot + 13] = $SectorsPerCluster
Set-U16 -Offset ($boot + 14) -Value $ReservedSectors
$image[$boot + 16] = $FatCount
Set-U16 -Offset ($boot + 17) -Value 0          # no fixed root directory on FAT32
Set-U16 -Offset ($boot + 19) -Value 0          # the 16-bit sector count is unused
$image[$boot + 21] = 0xF8                      # fixed disk
Set-U16 -Offset ($boot + 22) -Value 0          # the 16-bit FAT size is unused
Set-U16 -Offset ($boot + 24) -Value 63
Set-U16 -Offset ($boot + 26) -Value 255
Set-U32 -Offset ($boot + 28) -Value ([uint32]$PartitionStart)
Set-U32 -Offset ($boot + 32) -Value ([uint32]$PartitionSectors)
Set-U32 -Offset ($boot + 36) -Value ([uint32]$FatSectors)
Set-U16 -Offset ($boot + 40) -Value 0          # both FATs are live and mirrored
Set-U16 -Offset ($boot + 42) -Value 0          # filesystem version
Set-U32 -Offset ($boot + 44) -Value 2          # the root directory starts at cluster 2
Set-U16 -Offset ($boot + 48) -Value 1          # FSInfo
Set-U16 -Offset ($boot + 50) -Value 6          # backup boot sector
$image[$boot + 64] = 0x80
$image[$boot + 66] = 0x29                      # the extended fields below are present
Set-U32 -Offset ($boot + 67) -Value ([uint32]0x4E455855)   # "NEXU"
Set-Bytes -Offset ($boot + 71) -Value ([System.Text.Encoding]::ASCII.GetBytes('NEXUSOS    '))
Set-Bytes -Offset ($boot + 82) -Value ([System.Text.Encoding]::ASCII.GetBytes('FAT32   '))
Set-U16 -Offset ($boot + 510) -Value 0xAA55

$backupBoot = ($PartitionStart + 6) * $SectorSize
[Array]::Copy($image, $boot, $image, $backupBoot, $SectorSize)

# FSInfo: advisory, and firmware complains without it.
$fsinfo = ($PartitionStart + 1) * $SectorSize
Clear-Range -Offset $fsinfo -Length $SectorSize
Set-U32 -Offset $fsinfo -Value ([uint32]0x41615252)
Set-U32 -Offset ($fsinfo + 484) -Value ([uint32]0x61417272)
Set-U32 -Offset ($fsinfo + 488) -Value ([uint32]0xFFFFFFFFL)   # free count unknown
Set-U32 -Offset ($fsinfo + 492) -Value ([uint32]0xFFFFFFFFL)   # next free unknown
Set-U16 -Offset ($fsinfo + 510) -Value 0xAA55

# Both FATs start zeroed; entries are written as clusters are allocated.
Clear-Range -Offset ($FatStart * $SectorSize) -Length ($FatCount * $FatSectors * $SectorSize)

$script:NextCluster = 2
function New-ClusterChain {
    param([int]$Count)
    if ($Count -lt 1) { $Count = 1 }
    $first = $script:NextCluster
    for ($i = 0; $i -lt $Count; $i++) {
        $cluster = $first + $i
        $value = if ($i -eq $Count - 1) { [uint32]0x0FFFFFFF } else { [uint32]($cluster + 1) }
        for ($fat = 0; $fat -lt $FatCount; $fat++) {
            $offset = ($FatStart + $fat * $FatSectors) * $SectorSize + $cluster * 4
            Set-U32 -Offset $offset -Value $value
        }
    }
    $script:NextCluster += $Count
    if ($script:NextCluster -ge $ClusterCount + 2) { throw 'the filesystem is full' }
    return $first
}

function Get-ClusterOffset { param([int]$Cluster)
    return ($DataStart + ($Cluster - 2) * $SectorsPerCluster) * $SectorSize
}

$ClusterBytes = $SectorsPerCluster * $SectorSize

# Reserved entries: the first two FAT slots are not clusters.
for ($fat = 0; $fat -lt $FatCount; $fat++) {
    $offset = ($FatStart + $fat * $FatSectors) * $SectorSize
    Set-U32 -Offset $offset -Value ([uint32]0x0FFFFFF8)
    Set-U32 -Offset ($offset + 4) -Value ([uint32]0x0FFFFFFF)
}

# The root directory is one cluster. That bounds how many entries it can hold,
# which is checked when one is added.
$rootCluster = New-ClusterChain -Count 1
if ($rootCluster -ne 2) { throw "the root directory landed on cluster $rootCluster, not 2" }
Clear-Range -Offset (Get-ClusterOffset $rootCluster) -Length $ClusterBytes

# A directory is a cluster of 32-byte entries. `$script:DirNext` tracks how full
# each one is, keyed by its first cluster.
$script:DirNext = @{}
$script:DirNext[$rootCluster] = 0

function Add-DirectoryEntry {
    param(
        [int]$Directory,
        [string]$ShortName,      # eight characters, space padded
        [string]$Extension,      # three characters, space padded
        [byte]$Attributes,
        [int]$FirstCluster,
        [uint32]$Size
    )

    $index = $script:DirNext[$Directory]
    # One cluster per directory, so the capacity is however many 32-byte entries
    # fit in one. A tree that outgrows it says so rather than overwriting the
    # cluster after.
    if ($index -ge $ClusterBytes / 32) {
        throw "the directory is full at $index entries"
    }
    $script:DirNext[$Directory] = $index + 1

    $entry = (Get-ClusterOffset $Directory) + $index * 32
    Set-Bytes -Offset $entry -Value ([System.Text.Encoding]::ASCII.GetBytes($ShortName))
    Set-Bytes -Offset ($entry + 8) -Value ([System.Text.Encoding]::ASCII.GetBytes($Extension))
    $image[$entry + 11] = $Attributes
    # A fixed timestamp, so that building the image twice gives the same bytes.
    Set-U16 -Offset ($entry + 22) -Value 0x6000        # 12:00:00
    Set-U16 -Offset ($entry + 24) -Value 0x5921        # 2024-09-01
    Set-U16 -Offset ($entry + 20) -Value (($FirstCluster -shr 16) -band 0xFFFF)
    Set-U16 -Offset ($entry + 26) -Value ($FirstCluster -band 0xFFFF)
    Set-U32 -Offset ($entry + 28) -Value $Size
}

function Split-ShortName {
    param([string]$Name)
    $upper = $Name.ToUpperInvariant()
    $stem = $upper
    $extension = ''
    $dot = $upper.LastIndexOf('.')
    if ($dot -ge 0) {
        $stem = $upper.Substring(0, $dot)
        $extension = $upper.Substring($dot + 1)
    }
    if ($stem.Length -gt 8 -or $extension.Length -gt 3) {
        throw "the name '$Name' does not fit 8.3"
    }
    return @($stem.PadRight(8), $extension.PadRight(3))
}

function New-Directory {
    param([int]$Parent, [string]$Name)

    $cluster = New-ClusterChain -Count 1
    Clear-Range -Offset (Get-ClusterOffset $cluster) -Length $ClusterBytes
    $script:DirNext[$cluster] = 0

    $parts = Split-ShortName -Name $Name
    Add-DirectoryEntry -Directory $Parent -ShortName $parts[0] -Extension $parts[1] `
        -Attributes 0x10 -FirstCluster $cluster -Size 0

    # `.` and `..`, which every subdirectory carries. The parent of a directory
    # in the root is written as cluster zero, not two: that is what the
    # specification says and what readers check.
    $parentLink = if ($Parent -eq 2) { 0 } else { $Parent }
    Add-DirectoryEntry -Directory $cluster -ShortName '.       ' -Extension '   ' `
        -Attributes 0x10 -FirstCluster $cluster -Size 0
    Add-DirectoryEntry -Directory $cluster -ShortName '..      ' -Extension '   ' `
        -Attributes 0x10 -FirstCluster $parentLink -Size 0

    return $cluster
}

function Add-File {
    param([int]$Directory, [string]$Name, [byte[]]$Content)

    $clusters = [int][math]::Max(1, [math]::Ceiling($Content.Length / $ClusterBytes))
    $first = New-ClusterChain -Count $clusters
    Clear-Range -Offset (Get-ClusterOffset $first) -Length ($clusters * $ClusterBytes)
    if ($Content.Length -gt 0) {
        Set-Bytes -Offset (Get-ClusterOffset $first) -Value $Content
    }

    $parts = Split-ShortName -Name $Name
    Add-DirectoryEntry -Directory $Directory -ShortName $parts[0] -Extension $parts[1] `
        -Attributes 0x20 -FirstCluster $first -Size ([uint32]$Content.Length)
}

# The volume label lives in the root directory as an entry with no data.
Add-DirectoryEntry -Directory $rootCluster -ShortName 'NEXUSOS ' -Extension '   ' `
    -Attributes 0x08 -FirstCluster 0 -Size 0

# A file the filesystem reader is tested against. Its contents are fixed and
# its name is fixed, so a reader that found *a* file rather than *the* file is
# caught by what it says.
$greeting = "NexusOS reads its own filesystem.`r`n"
Add-File -Directory $rootCluster -Name 'HELLO.TXT' `
    -Content ([System.Text.Encoding]::ASCII.GetBytes($greeting))

# And one inside a directory, so that walking a path is something the reader is
# tested on rather than something it merely contains code for. A file in the
# root exercises none of it.
# Named so it cannot collide with anything in a tree copied in below. It once
# was `NEXUS`, which is also what the ESP calls the directory holding the
# kernel, and the firmware found this one first and reported no kernel at all.
$deepDirectory = New-Directory -Parent $rootCluster -Name 'TESTS'
$deep = "This file is one directory down.`r`n"
Add-File -Directory $deepDirectory -Name 'DEEP.TXT' `
    -Content ([System.Text.Encoding]::ASCII.GetBytes($deep))

# A file longer than one cluster, so that following a chain is tested too. Its
# contents are position-dependent, so a reader that stitched the clusters
# together in the wrong order fails rather than returning the right length.
$long = New-Object byte[] 5000
for ($i = 0; $i -lt $long.Length; $i++) {
    $long[$i] = [byte](($i * 31 + 7) -band 0xFF)
}
Add-File -Directory $rootCluster -Name 'CHAIN.BIN' -Content $long

# Programs, under `BIN`. The kernel reads them from here; the firmware does not
# look at them.
if ($ProgramDir -and (Test-Path $ProgramDir)) {
    $binDirectory = New-Directory -Parent $rootCluster -Name 'BIN'
    foreach ($item in Get-ChildItem -Path $ProgramDir -File | Sort-Object Name) {
        Add-File -Directory $binDirectory -Name $item.Name `
            -Content ([System.IO.File]::ReadAllBytes($item.FullName))
    }
}

# And the tree the firmware boots from, if one was given.
if ($SourceDir -and (Test-Path $SourceDir)) {
    function Copy-Tree {
        param([string]$Path, [int]$Directory)
        foreach ($item in Get-ChildItem -Path $Path | Sort-Object Name) {
            if ($item.PSIsContainer) {
                $child = New-Directory -Parent $Directory -Name $item.Name
                Copy-Tree -Path $item.FullName -Directory $child
            } else {
                Add-File -Directory $Directory -Name $item.Name `
                    -Content ([System.IO.File]::ReadAllBytes($item.FullName))
            }
        }
    }
    Copy-Tree -Path $SourceDir -Directory $rootCluster
}

# ---------------------------------------------------------------------------
# The partition table, written last so it describes what is there.
# ---------------------------------------------------------------------------

# A protective MBR, so a tool that only understands MBRs sees one partition
# covering the disk rather than empty space it might offer to format.
Clear-Range -Offset 0 -Length $SectorSize
$mbr = 446
$image[$mbr + 4] = 0xEE                                   # GPT protective
$image[$mbr + 1] = 0x00; $image[$mbr + 2] = 0x02; $image[$mbr + 3] = 0x00
$image[$mbr + 5] = 0xFF; $image[$mbr + 6] = 0xFF; $image[$mbr + 7] = 0xFF
Set-U32 -Offset ($mbr + 8) -Value 1
Set-U32 -Offset ($mbr + 12) -Value ([uint32][math]::Min([long]($TotalSectors - 1), $Mask32))
Set-U16 -Offset 510 -Value 0xAA55

# The one partition entry: an EFI system partition covering the FAT32.
$EntryLba = 2
$EntryCount = 128
$EntrySize = 128
$entries = $EntryLba * $SectorSize
Clear-Range -Offset $entries -Length ($EntryCount * $EntrySize)

# C12A7328-F81F-11D2-BA4B-00A0C93EC93B, in the mixed-endian form GPT uses.
$EspType = [byte[]](0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11,
    0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B)
# Fixed identifiers, so two builds of the same tree give the same image.
$PartitionGuid = [byte[]](0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
    0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10)
$DiskGuid = [byte[]](0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
    0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20)

Set-Bytes -Offset $entries -Value $EspType
Set-Bytes -Offset ($entries + 16) -Value $PartitionGuid
Set-U64 -Offset ($entries + 32) -Value ([uint64]$PartitionStart)
Set-U64 -Offset ($entries + 40) -Value ([uint64]$PartitionEnd)
Set-U64 -Offset ($entries + 48) -Value ([uint64]0)
$name = [System.Text.Encoding]::Unicode.GetBytes('NexusOS System')
Set-Bytes -Offset ($entries + 56) -Value $name

$entriesCrc = Get-Crc32 -Data $image -Offset $entries -Length ($EntryCount * $EntrySize)

function Write-GptHeader {
    param([int]$Lba, [int]$Alternate, [int]$EntriesLba)

    $header = $Lba * $SectorSize
    Clear-Range -Offset $header -Length $SectorSize
    Set-Bytes -Offset $header -Value ([System.Text.Encoding]::ASCII.GetBytes('EFI PART'))
    Set-U32 -Offset ($header + 8) -Value ([uint32]0x00010000)
    Set-U32 -Offset ($header + 12) -Value 92
    Set-U32 -Offset ($header + 16) -Value 0            # the CRC, computed below
    Set-U32 -Offset ($header + 20) -Value 0
    Set-U64 -Offset ($header + 24) -Value ([uint64]$Lba)
    Set-U64 -Offset ($header + 32) -Value ([uint64]$Alternate)
    Set-U64 -Offset ($header + 40) -Value ([uint64]34)
    Set-U64 -Offset ($header + 48) -Value ([uint64]($TotalSectors - 34))
    Set-Bytes -Offset ($header + 56) -Value $DiskGuid
    Set-U64 -Offset ($header + 72) -Value ([uint64]$EntriesLba)
    Set-U32 -Offset ($header + 80) -Value ([uint32]$EntryCount)
    Set-U32 -Offset ($header + 84) -Value ([uint32]$EntrySize)
    Set-U32 -Offset ($header + 88) -Value $entriesCrc

    # The header's own checksum covers exactly its 92 bytes, with the checksum
    # field zero -- which is why it is written last.
    $crc = Get-Crc32 -Data $image -Offset $header -Length 92
    Set-U32 -Offset ($header + 16) -Value $crc
}

$BackupEntriesLba = $TotalSectors - 33
[Array]::Copy($image, $entries, $image, $BackupEntriesLba * $SectorSize, $EntryCount * $EntrySize)

Write-GptHeader -Lba 1 -Alternate ($TotalSectors - 1) -EntriesLba $EntryLba
Write-GptHeader -Lba ($TotalSectors - 1) -Alternate 1 -EntriesLba $BackupEntriesLba

[System.IO.File]::WriteAllBytes($Output, $image)

$used = ($script:NextCluster - 2) * $ClusterBytes
Write-Host ("  disk       : {0} MiB, GPT + FAT32 ({1} clusters of {2} KiB, {3} KiB used)  -> {4}" -f `
        $SizeMiB, $ClusterCount, ($ClusterBytes / 1024), ($used / 1024), (Split-Path -Leaf $Output))
