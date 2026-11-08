<#
.SYNOPSIS
    Draws the NexusOS mark and saves it as a PNG, for the picture viewer to
    have something to show on a fresh machine.

.DESCRIPTION
    A machine whose picture viewer had nothing to show until somebody had
    already put a file on it would be a machine where that program cannot be
    used until it has been used. So one picture ships with the image.

    It is the same mark the boot logo draws -- a node with twelve spokes --
    which makes it worth having for a second reason: the boot logo is drawn by
    the kernel in code, and this is drawn by a real encoder and read back by
    this system's own decoder. If they look the same, both are right.

    Written with System.Drawing, which is on every Windows build machine, rather
    than by a PNG encoder written here. What is being tested is the *decoder*,
    and a decoder checked against pictures written by its own encoder is a
    decoder that agrees with itself.

.PARAMETER OutputFile
    Where to write the PNG.

.PARAMETER Size
    How many pixels across and down.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$OutputFile,
    [int]$Size = 512,
    [string]$Kind = 'Logo'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

Add-Type -AssemblyName System.Drawing

$bitmap = New-Object System.Drawing.Bitmap($Size, $Size, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAliasGridFit

    if ($Kind -eq 'Aurora') {
        # Deep arctic twilight gradient
        $skyTop = [System.Drawing.Color]::FromArgb(255, 6, 12, 28)
        $skyBottom = [System.Drawing.Color]::FromArgb(255, 12, 26, 44)
        $area = New-Object System.Drawing.Rectangle(0, 0, $Size, $Size)
        $gradient = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
            $area, $skyTop, $skyBottom, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
        $graphics.FillRectangle($gradient, $area)
        $gradient.Dispose()

        # Twinkling stars
        $starBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(220, 240, 250, 255))
        for ($i = 0; $i -lt 80; $i++) {
            $sx = ($i * 73 + 29) % $Size
            $sy = ($i * 59 + 17) % [int]($Size * 0.7)
            $sz = if ($i % 5 -eq 0) { 3 } else { 1.5 }
            $graphics.FillEllipse($starBrush, [float]$sx, [float]$sy, [float]$sz, [float]$sz)
        }
        $starBrush.Dispose()

        # Aurora borealis ribbons
        $pts1 = New-Object 'System.Collections.Generic.List[System.Drawing.PointF]'
        $pts2 = New-Object 'System.Collections.Generic.List[System.Drawing.PointF]'
        for ($x = 0; $x -le $Size; $x += 16) {
            $y1 = [float]($Size * 0.42 + [math]::Sin($x * 0.015) * 40 + [math]::Cos($x * 0.035) * 20)
            $y2 = [float]($Size * 0.48 + [math]::Sin($x * 0.02 + 1.2) * 35 + [math]::Cos($x * 0.04) * 15)
            $pts1.Add([System.Drawing.PointF]::new([float]$x, [float]$y1))
            $pts2.Add([System.Drawing.PointF]::new([float]$x, [float]$y2))
        }

        $pen1 = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(120, 52, 211, 153), 28.0)
        $graphics.DrawCurve($pen1, $pts1.ToArray())
        $pen1.Dispose()

        $pen2 = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(160, 56, 189, 248), 18.0)
        $graphics.DrawCurve($pen2, $pts2.ToArray())
        $pen2.Dispose()

        $pen3 = New-Object System.Drawing.Pen([System.Drawing.Color]::FromArgb(100, 168, 85, 247), 12.0)
        $graphics.DrawCurve($pen3, $pts1.ToArray())
        $pen3.Dispose()

        # Distant mountain silhouettes at bottom
        $mountPoints = @(
            [System.Drawing.PointF]::new(0, [float]$Size),
            [System.Drawing.PointF]::new(0, [float]($Size * 0.78)),
            [System.Drawing.PointF]::new([float]($Size * 0.22), [float]($Size * 0.68)),
            [System.Drawing.PointF]::new([float]($Size * 0.45), [float]($Size * 0.75)),
            [System.Drawing.PointF]::new([float]($Size * 0.7), [float]($Size * 0.65)),
            [System.Drawing.PointF]::new([float]($Size * 0.88), [float]($Size * 0.72)),
            [System.Drawing.PointF]::new([float]$Size, [float]($Size * 0.69)),
            [System.Drawing.PointF]::new([float]$Size, [float]$Size)
        )
        $mountBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 5, 10, 18))
        $graphics.FillPolygon($mountBrush, $mountPoints)
        $mountBrush.Dispose()
    } else {
        # The same two colours the machine boots in.
        $top = [System.Drawing.Color]::FromArgb(255, 11, 20, 40)
        $bottom = [System.Drawing.Color]::FromArgb(255, 4, 8, 20)
        $accent = [System.Drawing.Color]::FromArgb(255, 56, 139, 232)

        $area = New-Object System.Drawing.Rectangle(0, 0, $Size, $Size)
        $gradient = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
            $area, $top, $bottom, [System.Drawing.Drawing2D.LinearGradientMode]::Vertical)
        $graphics.FillRectangle($gradient, $area)
        $gradient.Dispose()

        $centre = $Size / 2.0
        $reach = $Size * 0.36

        # Twelve spokes from the middle, each ending in a point. The lengths
        # alternate so the shape reads as a node rather than as a wheel.
        $pen = New-Object System.Drawing.Pen($accent, [float]($Size / 128.0))
        $dot = New-Object System.Drawing.SolidBrush($accent)
        for ($spoke = 0; $spoke -lt 12; $spoke++) {
            $angle = $spoke * [math]::PI / 6.0
            $length = if ($spoke % 2 -eq 0) { $reach } else { $reach * 0.68 }
            $x = $centre + [math]::Cos($angle) * $length
            $y = $centre + [math]::Sin($angle) * $length
            $graphics.DrawLine($pen, [float]$centre, [float]$centre, [float]$x, [float]$y)
            $radius = $Size / 64.0
            $graphics.FillEllipse($dot, [float]($x - $radius), [float]($y - $radius),
                [float]($radius * 2), [float]($radius * 2))
        }

        # The node itself.
        $core = $Size * 0.075
        $graphics.FillEllipse($dot, [float]($centre - $core), [float]($centre - $core),
            [float]($core * 2), [float]($core * 2))
        $pen.Dispose()
        $dot.Dispose()

        $name = New-Object System.Drawing.Font('Segoe UI', [float]($Size / 14.0),
            [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
        $ink = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 230, 236, 245))
        $format = New-Object System.Drawing.StringFormat
        $format.Alignment = [System.Drawing.StringAlignment]::Center
        $graphics.DrawString('NexusOS', $name, $ink,
            [float]$centre, [float]($Size * 0.82), $format)
        $name.Dispose()
        $ink.Dispose()
        $format.Dispose()
    }

    $parent = Split-Path -Parent $OutputFile
    if ($parent -and -not (Test-Path $parent)) {
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
    }
    $bitmap.Save($OutputFile, [System.Drawing.Imaging.ImageFormat]::Png)

    # And the same picture as a JPEG, so the viewer has one of each to show and
    # the decoder is exercised on a real file rather than only on a fixture.
    # Quality 92: high enough that the mark stays clean, low enough that the
    # file is genuinely compressed rather than nearly raw.
    $jpeg = [System.IO.Path]::ChangeExtension($OutputFile, 'jpg')
    $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() |
        Where-Object { $_.MimeType -eq 'image/jpeg' }
    $settings = New-Object System.Drawing.Imaging.EncoderParameters(1)
    $settings.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter(
        [System.Drawing.Imaging.Encoder]::Quality, [long]92)
    $bitmap.Save($jpeg, $codec, $settings)
} finally {
    $graphics.Dispose()
    $bitmap.Dispose()
}

$bytes = (Get-Item $OutputFile).Length
Write-Host "    picture    : $([math]::Round($bytes / 1KB, 1)) KiB  -> $(Split-Path -Leaf $OutputFile)" -ForegroundColor DarkGray
