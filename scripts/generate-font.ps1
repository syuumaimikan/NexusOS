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
    Font to use. By default the best-scoring of a preference list.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$CharsetFile,
    [Parameter(Mandatory = $true)][string]$OutputFile,
    [string]$FontName,
    # Rasterise with grayscale anti-aliasing instead of GDI's hinted bitmaps.
    #
    # This matters more than it sounds. MS Gothic -- the default face -- carries
    # hand-tuned embedded bitmaps at sixteen pixels, and an embedded bitmap has
    # no anti-aliasing in it: every pixel is on or off. Capturing coverage from
    # it gives a table of zeros and fifteens, which is the crisp face and is
    # exactly right for what it is.
    #
    # A face with soft edges has to be rendered a different way: GDI+ rather
    # than GDI, and the hint that asks for grey coverage rather than for the
    # embedded bitmap. That is what this switch does, and it is why there are
    # two faces rather than one face with a flag.
    [switch]$Smooth
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
# is exactly what a 16-pixel cell wants, and it is monospaced, so its half-width
# forms actually fit eight columns. The other fixed-pitch CJK faces follow, then
# the proportional ones as a last resort.
$Preferred = @(
    'MS Gothic', 'MS Mincho', 'NSimSun', 'SimSun', 'Consolas', 'Courier New',
    'MS UI Gothic', 'Yu Gothic UI', 'Meiryo', 'Segoe UI'
)

# Widest characters of each class, used to choose an em size. If these fit, the
# rest do.
$HalfProbes = @('M', 'W', '@', 'N', 'O')
$FullProbes = @([char]0x4E2D, [char]0x7A3C, [char]0x8A9E, [char]0x30E1)
# Cap height is measured from this one: it is flat-topped and flat-bottomed, so
# its ink height is the cap height and not an overshoot.
$CapProbe = 'M'

# Em sizes to try, largest first. The largest that fits wins.
$EmSizes = 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8

$white = [System.Drawing.Color]::White
$black = [System.Drawing.Color]::Black
# NoPadding keeps GDI from inserting its usual leading, which would shift every
# glyph right by an inconsistent amount.
$Flags = [System.Windows.Forms.TextFormatFlags]::NoPadding

$bitmap = New-Object System.Drawing.Bitmap($Scratch, $Scratch)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)

function New-ProbeFont {
    param([string]$Family, [int]$Em)
    return New-Object System.Drawing.Font(
        $Family, $Em, [System.Drawing.FontStyle]::Regular,
        [System.Drawing.GraphicsUnit]::Pixel)
}

# The family GDI+ substitutes when it is asked for one that is not installed.
# Asking for a deliberately impossible name is the only reliable way to learn
# it, and knowing it is what makes substitution detectable below.
$absent = New-ProbeFont -Family 'NexusOS Deliberately Absent Family' -Em 16
$SubstituteFamily = $absent.Name
$absent.Dispose()

# Is this family actually installed, and what does the system call it?
#
# Not `InstalledFontCollection`: it reports *localised* family names, so on a
# Japanese system MS Gothic enumerates as "ＭＳ ゴシック" and a `-contains
# 'MS Gothic'` test fails. That is exactly what happened here — the build ran
# under a ja-JP UI culture and an interactive shell under en-US, so the same
# script picked MS Gothic in one and fell through to proportional MS UI Gothic
# at 11px in the other, and the only symptom was a kernel with tiny glyphs.
#
# Constructing the font instead asks GDI+ to do the lookup it will do anyway.
# It accepts the invariant English name and reports back the localised one; when
# the family is missing it silently substitutes, which comparing against
# $SubstituteFamily catches.
function Resolve-FamilyName {
    param([string]$Requested)

    $font = New-ProbeFont -Family $Requested -Em 16
    $name = $font.Name
    $font.Dispose()

    if ($name -eq $script:SubstituteFamily -and $Requested -ne $script:SubstituteFamily) {
        return $null
    }
    return $name
}

# Draw one character and return its rows as a bit-per-pixel array.
#
# Ink is the brightest of the three channels, not one of them. With ClearType
# the channels carry *subpixel* coverage, so a thin vertical stroke can light
# red and blue while leaving green nearly dark; sampling green alone dropped the
# right-hand stem of every N and O and rendered the title as "NexusCS".
# How much ink covers each pixel, 0 to 15, everywhere in the scratch bitmap.
#
# White on black, so the grey level of a pixel *is* its coverage. Sixteen levels
# rather than 256 because that is as much as the eye asks for at this size and a
# quarter of the bytes: a full-width glyph is sixteen rows of sixteen nibbles,
# which is a hundred and twenty-eight bytes.
function Get-GlyphCoverage {
    param($Font, [char]$Character)

    $graphics.Clear($black)
    if ($Smooth) {
        $graphics.TextRenderingHint =
            [System.Drawing.Text.TextRenderingHint]::AntiAlias
        $brush = New-Object System.Drawing.SolidBrush($white)
        $graphics.DrawString([string]$Character, $Font, $brush,
            (New-Object System.Drawing.PointF(0, 0)),
            [System.Drawing.StringFormat]::GenericTypographic)
        $brush.Dispose()
    } else {
        [System.Windows.Forms.TextRenderer]::DrawText(
            $graphics, [string]$Character, $Font,
            (New-Object System.Drawing.Point(0, 0)), $white, $black, $Flags)
    }

    $coverage = New-Object 'object[]' $Scratch
    for ($y = 0; $y -lt $Scratch; $y++) {
        $line = New-Object 'int[]' 32
        for ($x = 0; $x -lt 32; $x++) {
            $pixel = $bitmap.GetPixel($x, $y)
            $ink = [Math]::Max($pixel.R, [Math]::Max($pixel.G, $pixel.B))
            # 0..255 down to 0..15, rounded to nearest, so that a fully inked
            # pixel reaches fifteen rather than stopping one short.
            $line[$x] = [int][math]::Floor(($ink * 15 + 127) / 255)
        }
        $coverage[$y] = $line
    }
    return $coverage
}

# The same glyph as one bit per pixel, which is what every measurement below
# uses: where the ink starts and stops does not depend on how soft its edges
# are, and thresholding here leaves the fitting logic exactly as it was.
function Get-GlyphRows {
    param($Font, [char]$Character, $Coverage)

    if ($null -eq $Coverage) {
        $Coverage = Get-GlyphCoverage -Font $Font -Character $Character
    }
    $rows = New-Object 'int[]' $Scratch
    for ($y = 0; $y -lt $Scratch; $y++) {
        $row = 0
        $line = $Coverage[$y]
        for ($x = 0; $x -lt 32; $x++) {
            # Six of fifteen is the old threshold of ninety out of 255.
            if ($line[$x] -ge 6) { $row = $row -bor (1 -shl (31 - $x)) }
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

# Number of rows between the topmost and bottommost inked pixel, or 0 if blank.
function Get-InkHeight {
    param($Rows)
    $top = -1
    $bottom = -1
    for ($y = 0; $y -lt $Scratch; $y++) {
        if ($Rows[$y] -ne 0) {
            if ($top -lt 0) { $top = $y }
            $bottom = $y
        }
    }
    if ($top -lt 0) { return 0 }
    return $bottom - $top + 1
}

# Largest em size of $Family whose ink fits the cell, and the cap height it
# gives. Returns $null when no size fits.
#
# An em size is not an advance width, and the relationship between them is a
# property of the font, not something to assume. Asking MS Gothic for a
# 16-pixel em produced glyphs nine pixels wide, one past the eight-pixel cell,
# which is what cut the stems off N and O. So: rasterise the widest characters
# of each class at each candidate size and take the largest size whose ink
# actually fits. Measuring the drawing rather than asking the font metrics is
# the only thing that answers the question being asked.
function Measure-Fit {
    param([string]$Family)

    foreach ($em in $EmSizes) {
        $probeFont = New-ProbeFont -Family $Family -Em $em

        $halfInk = 0
        foreach ($probe in $HalfProbes) {
            $right = Get-InkRight (Get-GlyphRows -Font $probeFont -Character $probe)
            if ($right -ge $halfInk) { $halfInk = $right + 1 }
        }
        $fullInk = 0
        foreach ($probe in $FullProbes) {
            $right = Get-InkRight (Get-GlyphRows -Font $probeFont -Character $probe)
            if ($right -ge $fullInk) { $fullInk = $right + 1 }
        }
        $capHeight = Get-InkHeight (Get-GlyphRows -Font $probeFont -Character $CapProbe)

        $probeFont.Dispose()

        Write-Verbose "  ${Family} at ${em}px: half ink $halfInk, full ink $fullInk, cap $capHeight"
        if ($halfInk -le $HalfWidth -and $fullInk -le $FullWidth -and $capHeight -gt 0) {
            return [pscustomobject]@{ Em = $em; CapHeight = $capHeight }
        }
    }
    return $null
}

# Pick on measured quality, not on position in the list.
#
# Preference order only breaks ties. A proportional face squeezed down until its
# widest half-width form fits eight columns ends up drawing seven-pixel capitals
# in a sixteen-pixel cell — legible in isolation, unreadable as a UI. Scoring by
# cap height picks the face that uses the cell, and makes the fallback path
# degrade gracefully instead of arbitrarily.
$candidates = if ($FontName) { @($FontName) } else { $Preferred }

$best = $null
foreach ($candidate in $candidates) {
    $family = Resolve-FamilyName -Requested $candidate
    if (-not $family) {
        Write-Verbose "${candidate}: not installed"
        continue
    }

    $fit = Measure-Fit -Family $candidate
    if (-not $fit) {
        Write-Verbose "${candidate}: no em size fits a ${FullWidth}x${CellHeight} cell"
        continue
    }

    Write-Verbose "${candidate} (${family}): $($fit.Em)px, cap height $($fit.CapHeight)"
    if (($null -eq $best) -or ($fit.CapHeight -gt $best.CapHeight)) {
        $best = [pscustomobject]@{
            Requested = $candidate
            Family    = $family
            Em        = $fit.Em
            CapHeight = $fit.CapHeight
        }
    }
}

if ($null -eq $best) {
    if ($FontName) { throw "font '$FontName' is not installed, or no em size of it fits a ${FullWidth}x${CellHeight} cell" }
    throw 'none of the preferred fonts are installed'
}

$resolved = $best.Requested
$description = "$($best.Family) at $($best.Em)px"
if ($best.Family -ne $best.Requested) {
    $description = "$($best.Family) [$($best.Requested)] at $($best.Em)px"
}
Write-Host "rasterising with $description, cap height $($best.CapHeight)"

$font = New-ProbeFont -Family $resolved -Em $best.Em

$text = [System.IO.File]::ReadAllText($CharsetFile, [System.Text.Encoding]::UTF8)

# A case-SENSITIVE set. PowerShell's -contains and -notcontains compare
# strings case-insensitively, so deduplicating with them silently discarded
# every lowercase letter whose uppercase had already been seen: 'a' looked like
# a duplicate of 'A'. The missing glyphs then fell back to the built-in 8x8
# face, which sits on a different baseline, and the only symptom was a title
# whose lowercase letters were a few pixels too low.
$seen = New-Object 'System.Collections.Generic.HashSet[char]'
$characters = New-Object 'System.Collections.Generic.List[char]'
foreach ($character in $text.ToCharArray()) {
    if ([int]$character -lt 0x20) { continue }
    if ($seen.Add($character)) { $characters.Add($character) }
}

# First pass: rasterise, and find the vertical extent of the ink.
$rendered = @{}
$inkTop = $Scratch
$inkBottom = -1

$covered = @{}
foreach ($character in $characters) {
    $coverage = Get-GlyphCoverage -Font $font -Character $character
    $rows = Get-GlyphRows -Font $font -Character $character -Coverage $coverage
    for ($y = 0; $y -lt $Scratch; $y++) {
        if ($rows[$y] -ne 0) {
            if ($y -lt $inkTop) { $inkTop = $y }
            if ($y -gt $inkBottom) { $inkBottom = $y }
        }
    }
    $rendered[$character] = $rows
    $covered[$character] = $coverage
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
$lines.Add("# font: $description, cell ${FullWidth}x${CellHeight}")
$lines.Add('# format: codepoint advance row0..row15')
$lines.Add('# each row is 16 hex digits, one per pixel, leftmost first, each')
$lines.Add('# 0..F saying how much ink covers that pixel')

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
    $coverage = $covered[$character]
    for ($y = 0; $y -lt $CellHeight; $y++) {
        $source = $y + $offset
        # Sixteen nibbles, leftmost pixel first. The scratch is 32 columns wide
        # and the cell keeps the leftmost sixteen.
        $digits = New-Object System.Text.StringBuilder
        for ($x = 0; $x -lt $FullWidth; $x++) {
            $value = 0
            if ($source -lt $Scratch) { $value = $coverage[$source][$x] }
            [void]$digits.Append(('{0:X1}' -f $value))
        }
        $fields.Add($digits.ToString())
    }
    $lines.Add([string]::Join(' ', $fields))
}

$graphics.Dispose()
$bitmap.Dispose()
$font.Dispose()

[System.IO.File]::WriteAllLines($OutputFile, $lines, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "wrote $($characters.Count) glyphs to $OutputFile"
