<#
.SYNOPSIS
    Verify the native glass desktop and Super shortcuts through run.ps1.
.DESCRIPTION
    Requires a staged, configured default disk. Does not change user settings.
    Captures real QEMU frames and keeps serial evidence; closes only test windows.
#>
[CmdletBinding()]
param([int]$Timeout = 120)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')
$Repo = Split-Path -Parent $PSScriptRoot
$Evidence = Join-Path $Repo ('build/desktop-check-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Evidence | Out-Null
$Serial = Join-Path $Repo 'build/serial.log'
$Port = Get-FreeMonitorPort
$Arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', (Join-Path $PSScriptRoot 'run.ps1'),
    '-Headless', '-Timeout', ($Timeout + 90), '-MonitorPort', $Port, '-HostHttpPort', 0)
$Quoted = @($Arguments | ForEach-Object { '"' + (($_ -replace '(\\*)"', '$1$1\"') -replace '(\\+)$', '$1$1') + '"' })
$Runner = Start-Process -FilePath (Get-Command powershell).Source -ArgumentList $Quoted -WindowStyle Hidden -PassThru `
    -RedirectStandardOutput (Join-Path $Evidence 'run.log') -RedirectStandardError (Join-Path $Evidence 'run-error.log')
# Windows PowerShell can otherwise release its temporary process handle and
# later report a null ExitCode even after WaitForExit succeeds.
$null = $Runner.Handle
$Connection = $null
$Writer = $null
function Wait-DesktopMarker([string]$Marker) {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Runner.HasExited) {
        if (Test-Path -LiteralPath $Serial) {
            $Log = (Get-Content -LiteralPath $Serial -Raw) -replace "`0", ''
            if ($Log -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|user fault|compositor: (PANIC|FAILED)|launch: (PANIC|FAILED)|shell: FAILED') {
                throw "Desktop guest failed; see $Serial"
            }
            if ($Log.Contains($Marker)) { return }
        }
        Start-Sleep -Milliseconds 250
    }
    throw "Missing '$Marker'; see $Evidence"
}
function Send-DesktopKey([string]$Key) {
    $Writer.WriteLine("sendkey $Key")
    Start-Sleep -Milliseconds 900
}
function Save-DesktopFrame([string]$Name) {
    Start-Sleep -Milliseconds 500
    $Ppm = Join-Path $Evidence "$Name.ppm"
    Invoke-Screendump -Writer $Writer -Path $Ppm
    Convert-PpmToPng -PpmPath $Ppm -PngPath (Join-Path $Evidence "$Name.png")
    # Only our exact capture file is disposable; keep its viewable PNG.
    Remove-Item -LiteralPath $Ppm -Force
}
try {
    # The old serial log may remain until the runner acquires its lock. Wait
    # for the new process's monitor before trusting any marker in that log.
    $Deadline = [DateTime]::UtcNow.AddSeconds(20)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Runner.HasExited) {
        try { $Connection = [Net.Sockets.TcpClient]::new('127.0.0.1', $Port); break }
        catch { Start-Sleep -Milliseconds 250 }
    }
    if ($null -eq $Connection) { throw "Runner did not open its monitor; see $Evidence" }
    $Writer = [IO.StreamWriter]::new($Connection.GetStream())
    $Writer.AutoFlush = $true
    Wait-DesktopMarker 'shell: took the strip along the bottom of the screen'
    Wait-DesktopMarker 'compositor: backdrop blur active (CPU)'
    Wait-DesktopMarker 'client: 4x MSAA resolved with per-sample depth'
    Save-DesktopFrame 'desktop'
    Send-DesktopKey 'meta_l-spc'
    Wait-DesktopMarker 'compositor: Super+Space opens the command bar'
    Wait-DesktopMarker 'launch: a window for starting things by name'
    Save-DesktopFrame 'command-bar'
    foreach ($Key in @('t', 'e', 'r')) { Send-DesktopKey $Key }
    Save-DesktopFrame 'command-search'
    Send-DesktopKey 'ret'
    Wait-DesktopMarker 'launch: asked for term'
    Wait-DesktopMarker 'compositor: started a terminal, and lent it the filesystem and the network'
    Save-DesktopFrame 'terminal'
    Send-DesktopKey 'meta_l-shift-q'
    Wait-DesktopMarker 'compositor: Super+Shift+Q requested closing'
    Wait-DesktopMarker 'term: accepted decoration close'
    Send-DesktopKey 'meta_l-ret'
    Wait-DesktopMarker 'compositor: Super+Enter opens a terminal'
    Send-DesktopKey 'meta_l-shift-q'
    Send-DesktopKey 'meta_l-c'
    Wait-DesktopMarker 'compositor: started the settings, and lent them the settings'
    Save-DesktopFrame 'appearance'
    Send-DesktopKey 'meta_l-shift-q'
    Send-DesktopKey 'meta_l-spc'
    foreach ($Key in @('n', 'e', 'x')) { Send-DesktopKey $Key }
    Save-DesktopFrame 'window-search'
    Send-DesktopKey 'ret'
    Wait-DesktopMarker 'launch: asked to switch to an open window'
    Wait-DesktopMarker 'compositor: command bar switched to an existing window'
    $Writer.WriteLine('quit')
    if (-not $Runner.WaitForExit(15000)) { throw 'Runner did not stop after monitor quit' }
    if ($Runner.ExitCode -ne 0) { throw "run.ps1 failed (exit $($Runner.ExitCode)); see $Evidence" }
    Write-Host "PASS: native 4x MSAA, blur, Super shortcuts, filtered launch, window switching and appearance; $Evidence"
} finally {
    if ($null -ne $Writer) {
        if (-not $Runner.HasExited) { try { $Writer.WriteLine('quit') } catch { } }
        $Writer.Dispose()
    }
    if ($null -ne $Connection) { $Connection.Dispose() }
    if (-not $Runner.HasExited) {
        if (-not $Runner.WaitForExit(15000)) { $Runner.Kill(); $Runner.WaitForExit(5000) | Out-Null }
    }
    if (Test-Path -LiteralPath $Serial) { Copy-Item -LiteralPath $Serial -Destination (Join-Path $Evidence 'serial.log') }
}
