<#
.SYNOPSIS
    Rasterises the glyphs NexusOS needs from a font installed on this machine.

.DESCRIPTION
    Reads a UTF-8 file of characters, draws each one, and writes a table of
    bitmaps for build.rs to compile into the kernel.

    Why rasterise at build time rather than ship a font: the kernel has no font
    file and no rasteriser, hand-authoring a CJK face is not realistic, and
    committing a system font's bitmaps would be redistributing it. Rendering
    from the build machine's own licensed copy is the same arrangement as
    linking against a system library, and nothing proprietary enters the
    repository.

    Output format, one line per glyph:

        <codepoint hex> <advance width> <16 rows of hex, MSB leftmost>

.PARAMETER CharsetFile
    UTF-8 file containing every character to rasterise.

.PARAMETER OutputFile
    Where to write the glyph table.

.PARAMETER FontName
    Font to use. By default the first of a preference list that is installed.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CharsetFile,
    [Parameter(Mandatory = $true)][string]$OutputFile,
    [string]$FontName
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

# The cell every glyph has to fit: 16 rows, full-width 16 columns, half-width 8.
$CellHeight = 16
$FullWidth = 16
$HalfWidth = 8
# Scratch bitmap, generously larger than the cell so an oversized candidate is
# measured rather than silently clipped.
$Scratch = 48

# MS Gothic first: it carries hand-tuned embedded bitmaps at small sizes, which
# is exactly what a 16-pixel cell wants. The rest are fallbacks for machines
# without it.
$Preferred = @('MS Gothic', 'MS UI Gothic', 'Yu Gothic UI', 'Meiryo', 'Segoe UI', 'Consolas')

# Widest characters of each class, used to choose an em size. If these fit, the
# rest do.
$HalfProbes = @('M', 'W', '@', 'N', 'O')
$FullProbes = @([char]0x4E2D, [char]0x7A3C, [char]0x8A9E, [char]0x30E1)

$white = [System.Drawing.Color]::White
$black = [System.Drawing.Color]::Black
# NoPadding keeps GDI from inserting its usual leading, which would shift every
# glyph right by an inconsistent amount.
$Flags = [System.Windows.Forms.TextFormatFlags]::NoPadding

function Resolve-Font {
    param([string]$Requested)

    $installed = New-Object System.Drawing.Text.InstalledFontCollection
    $available = $installed.Families | ForEach-Object { $_.Name }

    if ($Requested) {
        if ($available -contains $Requested) { return $Requested }
        throw "font '$Requested' is not installed"
    }
    foreach ($candidate in $Preferred) {
        if ($available -contains $candidate) { return $candidate }
    }
    throw 'none of the preferred fonts are installed'
}

# Draw one character and return its rows as a bit-per-pixel array.
#
# Ink is the brightest of the three channels, not one of them. With ClearType
# the channels carry *subpixel* coverage, so a thin vertical stroke can light
# red and blue while leaving green nearly dark; sampling green alone dropped the
# right-hand stem of every N and O and rendered the title as "NexusCS".
function Get-GlyphRows {
    param($Graphics, $Bitmap, $Font, [char]$Character)

    $Graphics.Clear($black)
    [System.Windows.Forms.TextRenderer]::DrawText(
        $Graphics, [string]$Character, $Font,
        (New-Object System.Drawing.Point(0, 0)), $white, $black, $Flags)

    $rows = New-Object 'int[]' $Scratch
    for ($y = 0; $y -lt $Scratch; $y++) {
        $row = 0
        for ($x = 0; $x -lt 32; $x++) {
            $pixel = $Bitmap.GetPixel($x, $y)
            $ink = [Math]::Max($pixel.R, [Math]::Max($pixel.G, $pixel.B))
            if ($ink -ge 90) { $row = $row -bor (1 -shl (31 - $x)) }
        }
        $rows[$y] = $row
    }
    return $rows
}

# Column of the rightmost inked pixel across a glyph, or -1 when blank.
function Get-InkRight {
    param($Rows)
    $right = -1
    foreach ($row in $Rows) {
        if ($row -eq 0) { continue }
        for ($x = 31; $x -ge 0; $x--) {
            if ($row -band (1 -shl (31 - $x))) {
                if ($x -gt $right) { $right = $x }
                break
            }
        }
    }
    return $right
}

$resolved = Resolve-Font -Requested $FontName

$bitmap = New-Object System.Drawing.Bitmap($Scratch, $Scratch)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)

# An em size is not an advance width, and the relationship between them is a
# property of the font, not something to assume. Asking MS Gothic for a
# 16-pixel em produced glyphs nine pixels wide, one past the eight-pixel cell,
# which is what cut the stems off N and O. So: rasterise the widest characters
# of each class at each candidate size and take the largest size whose ink
# actually fits. Measuring the drawing rather than asking the font metrics is
# the only thing that answers the question being asked.
$chosen = 0
foreach ($candidate in 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8) {
    $probeFont = New-Object System.Drawing.Font($resolved, $candidate, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)

    $halfInk = 0
    foreach ($probe in $HalfProbes) {
        $right = Get-InkRight (Get-GlyphRows -Graphics $graphics -Bitmap $bitmap -Font $probeFont -Character $probe)
        if ($right -ge $halfInk) { $halfInk = $right + 1 }
    }
    $fullInk = 0
    foreach ($probe in $FullProbes) {
        $right = Get-InkRight (Get-GlyphRows -Graphics $graphics -Bitmap $bitmap -Font $probeFont -Character $probe)
        if ($right -ge $fullInk) { $fullInk = $right + 1 }
    }
    $probeFont.Dispose()

    Write-Verbose "em ${candidate}px: half-width ink $halfInk, full-width ink $fullInk"
    if ($halfInk -le $HalfWidth -and $fullInk -le $FullWidth -and $fullInk -gt 0) {
        $chosen = $candidate
        break
    }
}

if ($chosen -eq 0) {
    throw "no em size of '$resolved' fits a ${FullWidth}x${CellHeight} cell"
}
Write-Verbose "rasterising with $resolved at ${chosen}px"

$font = New-Object System.Drawing.Font($resolved, $chosen, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)

$text = [System.IO.File]::ReadAllText($CharsetFile, [System.Text.Encoding]::UTF8)
$characters = @()
foreach ($character in $text.ToCharArray()) {
    if ([int]$character -lt 0x20) { continue }
    if ($characters -notcontains $character) { $characters += $character }
}

# First pass: rasterise, and find the vertical extent of the ink.
$rendered = @{}
$inkTop = $Scratch
$inkBottom = -1

foreach ($character in $characters) {
    $rows = Get-GlyphRows -Graphics $graphics -Bitmap $bitmap -Font $font -Character $character
    for ($y = 0; $y -lt $Scratch; $y++) {
        if ($rows[$y] -ne 0) {
            if ($y -lt $inkTop) { $inkTop = $y }
            if ($y -gt $inkBottom) { $inkBottom = $y }
        }
    }
    $rendered[$character] = $rows
}

if ($inkBottom -lt 0) { throw 'every glyph rasterised blank' }

# One shift for every glyph, so relative baselines survive. A per-glyph shift
# would align each character to its own ink and destroy the baseline, leaving
# commas floating and capitals sitting low.
$offset = $inkTop
$inkHeight = $inkBottom - $inkTop + 1
if ($inkHeight -gt $CellHeight) {
    Write-Warning "glyphs span $inkHeight rows; the tallest will be clipped to $CellHeight"
    # Bias towards the top, where the ascenders are; descenders lose first.
    $offset = $inkTop
}

$lines = New-Object System.Collections.Generic.List[string]
$lines.Add('# NexusOS glyph table')
$lines.Add("# font: $resolved at ${chosen}px, cell ${FullWidth}x${CellHeight}")
$lines.Add('# format: codepoint advance row0..row15 (hex, MSB leftmost)')

foreach ($character in $characters) {
    $code = [int]$character
    # Half-width for ASCII, full-width for everything else. The em size was
    # chosen so that each class's ink fits its cell, so this holds by
    # construction rather than by assumption.
    $advance = if ($code -lt 0x80) { $HalfWidth } else { $FullWidth }

    $rows = $rendered[$character]
    $fields = New-Object System.Collections.Generic.List[string]
    $fields.Add(('{0:X4}' -f $code))
    $fields.Add([string]$advance)
    for ($y = 0; $y -lt $CellHeight; $y++) {
        $source = $y + $offset
        $value = 0
        if ($source -lt $Scratch) {
            # The scratch rows are 32 bits wide; the cell keeps the leftmost 16.
            $value = ($rows[$source] -shr 16) -band 0xFFFF
        }
        $fields.Add(('{0:X4}' -f $value))
    }
    $lines.Add([string]::Join(' ', $fields))
}

$graphics.Dispose()
$bitmap.Dispose()
$font.Dispose()

[System.IO.File]::WriteAllLines($OutputFile, $lines, (New-Object System.Text.UTF8Encoding($false)))
Write-Verbose "wrote $($characters.Count) glyphs to $OutputFile"
