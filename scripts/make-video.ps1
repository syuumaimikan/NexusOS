<#
.SYNOPSIS
    Draws a short Motion-JPEG recording, so the viewer has something to play on
    a machine nobody has put a file on yet.

.DESCRIPTION
    The same reasoning as make-picture.ps1: a machine whose player cannot be
    used until somebody has already used it to put a file somewhere is a machine
    with a chicken-and-egg problem instead of a feature.

    It is the boot logo's mark, turning. Motion that is obviously motion --
    a still frame and a playing one cannot be confused, which is what a test
    needs and what a person glancing at it needs too.

    # Why this writes the container by hand

    ffmpeg would do this in one line, and the fixtures in
    shared/nexus-image/fixtures are ffmpeg's output for exactly that reason:
    testing a decoder against files its own project wrote is testing that it
    agrees with itself. But ffmpeg is not on every build machine and build.ps1
    must work on all of them, so the file that ships is written here.

    The division is deliberate and worth keeping: **ffmpeg's files prove the
    decoder is right, this file gives the machine something to play.** If this
    script and the reader ever agreed on a mistake, the host tests would still
    catch it.

    The frames themselves are JPEGs from System.Drawing, which is a third-party
    encoder, so even here the compressed data is not this project's own idea of
    what a JPEG is. Only the RIFF wrapper around them is.

.PARAMETER OutputFile
    Where to write the AVI.

.PARAMETER Width
    Frame width in pixels.

.PARAMETER Height
    Frame height in pixels.

.PARAMETER Frames
    How many frames. They loop, so this is also the period of the animation.

.PARAMETER Fps
    Frames per second.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputFile,
    [int]$Width = 240,
    [int]$Height = 180,
    [int]$Frames = 24,
    [int]$Fps = 12
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.Drawing

# ---------------------------------------------------------------------------
# The frames
# ---------------------------------------------------------------------------

$jpegCodec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() |
    Where-Object { $_.MimeType -eq 'image/jpeg' }
$jpegSettings = New-Object System.Drawing.Imaging.EncoderParameters(1)
$jpegSettings.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter(
    [System.Drawing.Imaging.Encoder]::Quality, [long]78)

$top = [System.Drawing.Color]::FromArgb(255, 11, 20, 40)
$bottom = [System.Drawing.Color]::FromArgb(255, 4, 8, 20)
$accent = [System.Drawing.Color]::FromArgb(255, 56, 139, 232)

$encoded = New-Object System.Collections.ArrayList

for ($frame = 0; $frame -lt $Frames; $frame++) {
    $bitmap = New-Object System.Drawing.Bitmap($Width, $Height,
        [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
        $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
        $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias

        $area = New-Object System.Drawing.Rectangle(0, 0, $Width, $Height)
        $gradient = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
            $area, $top, $bottom, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
        $graphics.FillRectangle($gradient, $area)
        $gradient.Dispose()

        $centreX = $Width / 2.0
        $centreY = $Height / 2.0
        $reach = [math]::Min($Width, $Height) * 0.34

        # One full turn over the whole clip, so the last frame leads back into
        # the first and the loop has no jump in it.
        $turn = 2.0 * [math]::PI * $frame / $Frames

        $pen = New-Object System.Drawing.Pen($accent, [float]($Width / 100.0))
        $dot = New-Object System.Drawing.SolidBrush($accent)
        for ($spoke = 0; $spoke -lt 12; $spoke++) {
            $angle = $turn + $spoke * [math]::PI / 6.0
            $length = if ($spoke % 2 -eq 0) { $reach } else { $reach * 0.68 }
            $x = $centreX + [math]::Cos($angle) * $length
            $y = $centreY + [math]::Sin($angle) * $length
            $graphics.DrawLine($pen, [float]$centreX, [float]$centreY, [float]$x, [float]$y)
            $radius = $Width / 80.0
            $graphics.FillEllipse($dot, [float]($x - $radius), [float]($y - $radius),
                [float]($radius * 2), [float]($radius * 2))
        }

        $core = [math]::Min($Width, $Height) * 0.07
        $graphics.FillEllipse($dot, [float]($centreX - $core), [float]($centreY - $core),
            [float]($core * 2), [float]($core * 2))
        $pen.Dispose()
        $dot.Dispose()

        # The frame number, so that a screenshot of a playing recording can be
        # told from a screenshot of a stopped one by looking at it.
        $font = New-Object System.Drawing.Font('Segoe UI', [float]($Height / 12.0),
            [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
        $ink = New-Object System.Drawing.SolidBrush(
            [System.Drawing.Color]::FromArgb(255, 230, 236, 245))
        $format = New-Object System.Drawing.StringFormat
        $format.Alignment = [System.Drawing.StringAlignment]::Center
        $graphics.DrawString("$($frame + 1) / $Frames", $font, $ink,
            [float]$centreX, [float]($Height * 0.86), $format)
        $font.Dispose()
        $ink.Dispose()
        $format.Dispose()

        $stream = New-Object System.IO.MemoryStream
        $bitmap.Save($stream, $jpegCodec, $jpegSettings)
        [void]$encoded.Add($stream.ToArray())
        $stream.Dispose()
    } finally {
        $graphics.Dispose()
        $bitmap.Dispose()
    }
}

# ---------------------------------------------------------------------------
# The container
# ---------------------------------------------------------------------------
#
# AVI is RIFF: four-byte tags, little-endian lengths, every chunk padded to an
# even byte. The padding is the part everybody gets wrong, so it is done in one
# place here -- Add-Chunk -- rather than at each call.

function New-Writer {
    $stream = New-Object System.IO.MemoryStream
    New-Object System.IO.BinaryWriter($stream)
}

function Get-Bytes {
    param($Writer)
    $Writer.Flush()
    $Writer.BaseStream.ToArray()
}

function Add-Chunk {
    param($Writer, [string]$Tag, [byte[]]$Body)
    $Writer.Write([System.Text.Encoding]::ASCII.GetBytes($Tag))
    $Writer.Write([uint32]$Body.Length)
    $Writer.Write($Body)
    # The pad byte is not counted in the length. A reader that adds the length
    # and steps on lands one byte early from the first odd chunk onwards.
    if ($Body.Length % 2 -eq 1) { $Writer.Write([byte]0) }
}

$microSecPerFrame = [uint32](1000000 / $Fps)
$largest = ($encoded | ForEach-Object { $_.Length } | Measure-Object -Maximum).Maximum

# avih: the main header, fourteen 32-bit fields.
$w = New-Writer
$w.Write([uint32]$microSecPerFrame)
$w.Write([uint32]($largest * $Fps))    # dwMaxBytesPerSec
$w.Write([uint32]0)                    # dwPaddingGranularity
$w.Write([uint32]0x10)                 # dwFlags: AVIF_HASINDEX
$w.Write([uint32]$encoded.Count)       # dwTotalFrames
$w.Write([uint32]0)                    # dwInitialFrames
$w.Write([uint32]1)                    # dwStreams
$w.Write([uint32]$largest)             # dwSuggestedBufferSize
$w.Write([uint32]$Width)
$w.Write([uint32]$Height)
$w.Write([uint32]0); $w.Write([uint32]0); $w.Write([uint32]0); $w.Write([uint32]0)
$avih = Get-Bytes $w
if ($avih.Length -ne 56) { throw "avih came to $($avih.Length) bytes, not 56" }

# strh: the stream header.
$w = New-Writer
$w.Write([System.Text.Encoding]::ASCII.GetBytes('vids'))
$w.Write([System.Text.Encoding]::ASCII.GetBytes('MJPG'))
$w.Write([uint32]0)                    # dwFlags
$w.Write([uint16]0)                    # wPriority
$w.Write([uint16]0)                    # wLanguage
$w.Write([uint32]0)                    # dwInitialFrames
$w.Write([uint32]1)                    # dwScale
$w.Write([uint32]$Fps)                 # dwRate: rate/scale is frames per second
$w.Write([uint32]0)                    # dwStart
$w.Write([uint32]$encoded.Count)       # dwLength
$w.Write([uint32]$largest)             # dwSuggestedBufferSize
$w.Write([uint32]0)                    # dwQuality
$w.Write([uint32]0)                    # dwSampleSize
$w.Write([int16]0); $w.Write([int16]0)
$w.Write([int16]$Width); $w.Write([int16]$Height)
$strh = Get-Bytes $w
if ($strh.Length -ne 56) { throw "strh came to $($strh.Length) bytes, not 56" }

# strf: a BITMAPINFOHEADER saying the frames are JPEGs.
$w = New-Writer
$w.Write([uint32]40)                   # biSize
$w.Write([int32]$Width)
$w.Write([int32]$Height)
$w.Write([uint16]1)                    # biPlanes
$w.Write([uint16]24)                   # biBitCount
$w.Write([System.Text.Encoding]::ASCII.GetBytes('MJPG'))   # biCompression
$w.Write([uint32]($Width * $Height * 3))                   # biSizeImage
$w.Write([int32]0); $w.Write([int32]0)                     # pixels per metre
$w.Write([uint32]0); $w.Write([uint32]0)                   # palette
$strf = Get-Bytes $w
if ($strf.Length -ne 40) { throw "strf came to $($strf.Length) bytes, not 40" }

# LIST strl { strh, strf }
$w = New-Writer
$w.Write([System.Text.Encoding]::ASCII.GetBytes('strl'))
Add-Chunk -Writer $w -Tag 'strh' -Body $strh
Add-Chunk -Writer $w -Tag 'strf' -Body $strf
$strl = Get-Bytes $w

# LIST hdrl { avih, LIST strl }
$w = New-Writer
$w.Write([System.Text.Encoding]::ASCII.GetBytes('hdrl'))
Add-Chunk -Writer $w -Tag 'avih' -Body $avih
Add-Chunk -Writer $w -Tag 'LIST' -Body $strl
$hdrl = Get-Bytes $w

# LIST movi { 00dc ... }, and the index that describes it.
#
# idx1 offsets are counted from the 'movi' tag itself, which is the convention
# every writer uses and the reason this system's reader ignores the index
# entirely and walks the chunks instead: the other convention -- absolute file
# offsets -- also exists, and a reader cannot tell which it is looking at.
$w = New-Writer
$w.Write([System.Text.Encoding]::ASCII.GetBytes('movi'))
$index = New-Writer
foreach ($frame in $encoded) {
    # Where this chunk's header will sit, relative to the 'movi' tag: the four
    # bytes of the tag itself, plus everything written after it so far.
    $offset = 4 + $w.BaseStream.Position - 4
    $index.Write([System.Text.Encoding]::ASCII.GetBytes('00dc'))
    $index.Write([uint32]0x10)         # AVIIF_KEYFRAME: every frame is one
    $index.Write([uint32]$offset)
    $index.Write([uint32]$frame.Length)
    Add-Chunk -Writer $w -Tag '00dc' -Body $frame
}
$movi = Get-Bytes $w
$idx1 = Get-Bytes $index

# RIFF AVI { LIST hdrl, LIST movi, idx1 }
$w = New-Writer
$w.Write([System.Text.Encoding]::ASCII.GetBytes('AVI '))
Add-Chunk -Writer $w -Tag 'LIST' -Body $hdrl
Add-Chunk -Writer $w -Tag 'LIST' -Body $movi
Add-Chunk -Writer $w -Tag 'idx1' -Body $idx1
$body = Get-Bytes $w

$w = New-Writer
Add-Chunk -Writer $w -Tag 'RIFF' -Body $body
$file = Get-Bytes $w

$parent = Split-Path -Parent $OutputFile
if ($parent -and -not (Test-Path $parent)) {
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
}
[System.IO.File]::WriteAllBytes($OutputFile, $file)

$kib = [math]::Round($file.Length / 1KB, 1)
Write-Host "    recording  : $kib KiB, $($encoded.Count) frames of ${Width}x${Height} at $Fps a second" -ForegroundColor DarkGray
