<#
.SYNOPSIS
    Screen capture helpers shared by the screenshot scripts.

.DESCRIPTION
    Dot-source this from a script that has QEMU's monitor open:

        . (Join-Path $PSScriptRoot 'capture.ps1')

    Nothing here starts or stops QEMU; the caller owns the process.
#>

Set-StrictMode -Version Latest

# Wait for a file to appear and stop growing.
#
# A screendump at this resolution is about seven megabytes, and how long QEMU
# takes to write it depends on the host. Polling for a size that has stopped
# changing says the capture is finished; a fixed sleep only says a plausible
# interval has passed, and would hand a half-written PPM to the converter on a
# busy machine.
function Wait-ForStableFile {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [int]$TimeoutSeconds = 20
    )

    $previous = -1
    $stable = 0
    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)

    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
        if (-not (Test-Path $Path)) { continue }

        $size = (Get-Item $Path).Length
        if ($size -gt 0 -and $size -eq $previous) {
            $stable++
            # Two consecutive equal readings, half a second apart. One is not
            # enough: the writer can be paused between blocks.
            if ($stable -ge 2) { return $true }
        } else {
            $stable = 0
        }
        $previous = $size
    }
    return $false
}

# Capture the guest's display to a PPM through an open QEMU monitor.
function Invoke-Screendump {
    param(
        [Parameter(Mandatory = $true)]$Writer,
        [Parameter(Mandatory = $true)][string]$Path
    )

    if (Test-Path $Path) { Remove-Item $Path -Force }

    $Writer.WriteLine("screendump $Path")
    if (-not (Wait-ForStableFile -Path $Path)) {
        throw 'QEMU never finished the screendump'
    }
}

# Convert the binary PPM QEMU writes into a PNG that ordinary tools read.
function Convert-PpmToPng {
    param(
        [Parameter(Mandatory = $true)][string]$PpmPath,
        [Parameter(Mandatory = $true)][string]$PngPath
    )

    Add-Type -AssemblyName System.Drawing
    $bytes = [System.IO.File]::ReadAllBytes($PpmPath)

    # P6, width, height, maxval -- whitespace separated, with `#` comments
    # allowed between any two of them.
    $pos = 0
    $tokens = New-Object System.Collections.Generic.List[string]
    while ($tokens.Count -lt 4 -and $pos -lt $bytes.Length) {
        while ($pos -lt $bytes.Length -and [char]$bytes[$pos] -match '\s') { $pos++ }
        if ($pos -lt $bytes.Length -and [char]$bytes[$pos] -eq '#') {
            while ($pos -lt $bytes.Length -and $bytes[$pos] -ne 10) { $pos++ }
            continue
        }
        $start = $pos
        while ($pos -lt $bytes.Length -and -not ([char]$bytes[$pos] -match '\s')) { $pos++ }
        $tokens.Add([System.Text.Encoding]::ASCII.GetString($bytes, $start, $pos - $start))
    }
    # The single whitespace character that ends the header; pixels follow.
    $pos++

    if ($tokens[0] -ne 'P6') { throw "Unexpected screendump format: $($tokens[0])" }
    $width = [int]$tokens[1]
    $height = [int]$tokens[2]

    $expected = $pos + $width * $height * 3
    if ($bytes.Length -lt $expected) {
        throw "screendump is truncated: $($bytes.Length) bytes, expected $expected"
    }

    $bitmap = New-Object System.Drawing.Bitmap($width, $height, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
    $rect = New-Object System.Drawing.Rectangle(0, 0, $width, $height)
    $data = $bitmap.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bitmap.PixelFormat)
    try {
        $row = New-Object byte[] $data.Stride
        for ($y = 0; $y -lt $height; $y++) {
            $src = $pos + $y * $width * 3
            for ($x = 0; $x -lt $width; $x++) {
                $i = $src + $x * 3
                $o = $x * 3
                # PPM is RGB; a 24bpp GDI+ bitmap is BGR.
                $row[$o]     = $bytes[$i + 2]
                $row[$o + 1] = $bytes[$i + 1]
                $row[$o + 2] = $bytes[$i]
            }
            [System.Runtime.InteropServices.Marshal]::Copy($row, 0, [IntPtr]($data.Scan0.ToInt64() + $y * $data.Stride), $data.Stride)
        }
    } finally {
        $bitmap.UnlockBits($data)
    }

    $bitmap.Save($PngPath, [System.Drawing.Imaging.ImageFormat]::Png)
    $bitmap.Dispose()

    return [pscustomobject]@{ Width = $width; Height = $height }
}
