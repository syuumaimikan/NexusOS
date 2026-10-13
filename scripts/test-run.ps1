<#
.SYNOPSIS
    Boot the staged desktop and verify launch failures, paths and disk locking.
.DESCRIPTION
    Requires a configured default build. Uses a disposable copy of its disk and
    ESP under a path containing spaces. Keeps results in build/run-check.log.
#>
[CmdletBinding()]
param([ValidateRange(10, 600)][int]$Timeout = 90)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
$repo = Split-Path -Parent $PSScriptRoot
$fixture = Join-Path $repo ('build/run-check-' + [guid]::NewGuid().ToString('N') + ' with spaces')
$runScript = Join-Path $PSScriptRoot 'run.ps1'
$result = Join-Path $repo 'build/run-check.log'
$listener = $null
$lock = $null
New-Item -ItemType Directory -Path $fixture | Out-Null
function Run-Check([string[]]$Options, [bool]$ShouldSucceed) {
    $ErrorActionPreference = 'Continue'
    try {
        & powershell -NoProfile -ExecutionPolicy Bypass -File $runScript -BuildDirectory $fixture -Headless @Options *> (Join-Path $fixture 'call.log')
    } finally { $ErrorActionPreference = 'Stop' }
    $exitCode = $LASTEXITCODE
    Get-Content -LiteralPath (Join-Path $fixture 'call.log') | Add-Content -LiteralPath $result
    if (($exitCode -eq 0) -ne $ShouldSucceed) { throw "Unexpected launch exit $exitCode; see $result" }
}
try {
    Copy-Item -LiteralPath (Join-Path $repo 'build/esp') -Destination (Join-Path $fixture 'esp') -Recurse
    Copy-Item -LiteralPath (Join-Path $repo 'build/nexus-disk.img') -Destination (Join-Path $fixture 'nexus-disk.img')
    Set-Content -LiteralPath $result -Value 'NexusOS launch regression checks'
    Push-Location $fixture
    try {
        Run-Check @('-Until', 'shell: took the strip along the bottom of the screen', '-Timeout', "$Timeout", '-HostHttpPort', '0') $true
    } finally { Pop-Location }
    Copy-Item -LiteralPath (Join-Path $fixture 'serial.log') -Destination (Join-Path $repo 'build/run-desktop.log') -Force
    Write-Host 'PASS: configured desktop boots from another working directory with spaces in every QEMU file path'
    Run-Check @('-Until', 'intentionally absent launch regression marker', '-Timeout', '2', '-HostHttpPort', '0') $false
    if (-not (Get-Content -LiteralPath (Join-Path $fixture 'call.log') -Raw).Contains('Did not reach')) { throw 'Missing-marker failure was not reported' }
    Write-Host 'PASS: a missing boot marker returns a failing process status'

    $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    Run-Check @('-Until', 'shell: took the strip along the bottom of the screen', '-Timeout', '10', '-HostHttpPort', "$port") $false
    if (-not (Get-Content -LiteralPath (Join-Path $fixture 'qemu-error.log') -Raw).Contains('host forwarding')) { throw 'QEMU failure diagnostics were not retained' }
    Write-Host 'PASS: QEMU port conflicts return failure with the original error log'

    $lock = [IO.File]::Open((Join-Path $fixture 'run.lock'), 'Open', 'ReadWrite', 'None')
    Set-Content -LiteralPath (Join-Path $fixture 'serial.log') -Value 'existing run evidence'
    Run-Check @('-Timeout', '1', '-HostHttpPort', '0') $false
    if ((Get-Content -LiteralPath (Join-Path $fixture 'serial.log') -Raw).Trim() -ne 'existing run evidence') { throw 'Concurrent launch replaced another run log' }
    Write-Host 'PASS: concurrent launch fails before replacing existing evidence'
} finally {
    if ($null -ne $lock) { $lock.Dispose() }
    if ($null -ne $listener) { $listener.Stop() }
    $safe = Assert-NexusGeneratedPath -Path $fixture -Root (Join-Path $repo 'build')
    Remove-Item -LiteralPath $safe -Recurse -Force
}
exit 0
