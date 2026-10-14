# Shared cleanup helpers. Only explicitly owned disposable test payloads are
# eligible; evidence, persistent machines and dependency caches stay in place.
Set-StrictMode -Version Latest

function Assert-NexusGeneratedPath {
    param([string]$Path, [string]$Root)
    $base = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    $full = [IO.Path]::GetFullPath($Path)
    if (-not $full.StartsWith($base + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Generated path is outside $base : $full"
    }
    # Never traverse a junction/symlink while removing generated trees.
    $cursor = $full
    while ($cursor.Length -ge $base.Length) {
        if (Test-Path -LiteralPath $cursor) {
            if ((Get-Item -LiteralPath $cursor -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing generated path through a reparse point: $cursor"
            }
        }
        $cursor = Split-Path -Parent $cursor
        if (-not $cursor) { break }
    }
    if (Test-Path -LiteralPath $full -PathType Container) {
        foreach ($entry in Get-ChildItem -LiteralPath $full -Recurse -Force) {
            if ($entry.Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw "Refusing generated tree containing a reparse point: $($entry.FullName)"
            }
        }
    }
    return $full
}

function Register-NexusDisposableRun {
    param([string]$Directory)
    $root = Join-Path (Split-Path -Parent $PSScriptRoot) 'build'
    $full = Assert-NexusGeneratedPath -Path $Directory -Root $root
    if ((Split-Path -Leaf $full) -notmatch '^(runtime-test|steam-diagnostic|linux-graphics|rtx-bootstrap)-[a-f0-9]{32}$') {
        throw "Not a disposable test directory: $full"
    }
    New-Item -ItemType Directory -Path $full -Force | Out-Null
    @{ Version = 1; OwnerPid = $PID; CreatedUtc = [DateTime]::UtcNow.ToString('o') } |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $full '.nexus-disposable.json') -Encoding UTF8
}

function Invoke-NexusCleanup {
    [CmdletBinding(SupportsShouldProcess)]
    param([ValidateRange(0, 100)][int]$Keep = 2, [string]$RootDirectory = '')
    $root = Join-Path (Split-Path -Parent $PSScriptRoot) 'build'
    if ($RootDirectory) { $root = Assert-NexusGeneratedPath -Path $RootDirectory -Root $root }
    if (-not (Test-Path -LiteralPath $root)) { return }
    $runs = @(Get-ChildItem -LiteralPath $root -Directory | Where-Object {
        $_.Name -match '^(runtime-test|steam-diagnostic|linux-graphics|rtx-bootstrap)-[a-f0-9]{32}$' -and
        (Test-Path -LiteralPath (Join-Path $_.FullName '.nexus-disposable.json'))
    })
    foreach ($family in @('runtime-test', 'steam-diagnostic', 'linux-graphics', 'rtx-bootstrap')) {
        $rank = 0
        foreach ($run in @($runs | Where-Object { $_.Name.StartsWith($family + '-') } | Sort-Object CreationTimeUtc -Descending)) {
            $rank++
            if ($rank -le $Keep) { continue }
            try {
                $owner = Get-Content -LiteralPath (Join-Path $run.FullName '.nexus-disposable.json') -Raw | ConvertFrom-Json
                if ($owner.Version -ne 1 -or $owner.OwnerPid -le 0) { continue }
                if (Get-Process -Id $owner.OwnerPid -ErrorAction SilentlyContinue) { continue }
                $payload = @(Get-ChildItem -LiteralPath $run.FullName -Force | Where-Object {
                    ($_.PSIsContainer -and $_.Name -in @('esp', 'programs')) -or
                    (-not $_.PSIsContainer -and ($_.Name -in @('nexus-disk.img', 'code.fd', 'vars.fd', 'edk2-x86_64-code.fd', 'edk2-vars.fd') -or $_.Extension -eq '.ppm'))
                })
                $targets = @($payload | ForEach-Object { Assert-NexusGeneratedPath -Path $_.FullName -Root $root })
                # If QEMU still owns any image, leave the entire run alone.
                $handles = @()
                try {
                    foreach ($file in @($payload | Where-Object { -not $_.PSIsContainer -and $_.Extension -in @('.img', '.fd') })) {
                        $handles += [IO.File]::Open($file.FullName, 'Open', 'ReadWrite', 'None')
                    }
                } finally {
                    foreach ($handle in $handles) { $handle.Dispose() }
                }
                foreach ($target in $targets) {
                    if ($PSCmdlet.ShouldProcess($target, 'Remove disposable test payload (retain evidence)')) {
                        Remove-Item -LiteralPath $target -Recurse -Force
                        Write-Host "    removed $target" -ForegroundColor DarkGray
                    }
                }
                $remaining = @(Get-ChildItem -LiteralPath $run.FullName -Force)
                if ($remaining.Count -eq 1 -and $remaining[0].Name -eq '.nexus-disposable.json') {
                    $empty = Assert-NexusGeneratedPath -Path $run.FullName -Root $root
                    if ($PSCmdlet.ShouldProcess($empty, 'Remove empty disposable run record')) {
                        Remove-Item -LiteralPath $empty -Recurse -Force
                    }
                }
            } catch {
                Write-Warning "Kept $($run.Name): $_"
            }
        }
    }
}
