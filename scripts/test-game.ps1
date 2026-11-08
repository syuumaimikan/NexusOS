<#
.SYNOPSIS
    Tests the Steamworks gaming subsystem and Spacewar game client on NexusOS.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 240
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$Log = Join-Path $BuildDir 'game-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-game.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 43000 -Maximum 43999
$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $FirmwareVars -SerialLog $Log `
    -MonitorPort $MonitorPort -Headless

Write-Host "==> Booting NexusOS with a monitor on port $MonitorPort" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

function Wait-For {
    param([string]$Text, [int]$Seconds)
    for ($waited = 0; $waited -lt $Seconds; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { return $false }
        if (Test-Path $Log) {
            $sofar = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains($Text)) { return $true }
        }
    }
    return $false
}

$failures = @()

try {
    Write-Host '==> Waiting for the desktop' -ForegroundColor Cyan
    if (-not (Wait-For -Text 'shell: took the strip' -Seconds $Timeout)) {
        throw 'the desktop never appeared'
    }

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        function Send-Keys {
            param([string[]]$Keys, [int]$Pause = 140)
            foreach ($key in $Keys) {
                $writer.WriteLine("sendkey $key")
                Start-Sleep -Milliseconds $Pause
            }
        }

        function Send-Text {
            param([string]$Text)
            foreach ($letter in $Text.ToCharArray()) {
                $key = switch ($letter) {
                    ' ' { 'spc' }
                    '.' { 'dot' }
                    '(' { 'shift-9' }
                    ')' { 'shift-0' }
                    '*' { 'shift-8' }
                    '+' { 'shift-equal' }
                    '"' { 'shift-apostrophe' }
                    '|' { 'shift-backslash' }
                    '/' { 'slash' }
                    '-' { 'minus' }
                    '=' { 'equal' }
                    ',' { 'comma' }
                    default { $letter }
                }
                Send-Keys @($key) -Pause 105
            }
        }

        Write-Host '==> Opening a terminal' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Send-Text 'tl'
        Start-Sleep -Seconds 1
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'compositor: started a terminal' -Seconds 60)) {
            throw 'the terminal never started'
        }
        Start-Sleep -Seconds 2

        Write-Host '==> Checking Steamworks gaming subsystem status (steam info)' -ForegroundColor Cyan
        Send-Text 'steam info'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'term: ran steam' -Seconds 30)) {
            $failures += 'the shell never ran steam info'
        }
        Start-Sleep -Seconds 1

        Write-Host '==> Listing Steam games (steam list)' -ForegroundColor Cyan
        Send-Text 'steam list'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'term: ran steam' -Seconds 30)) {
            $failures += 'the shell never ran steam list'
        }
        Start-Sleep -Seconds 1

        Write-Host '==> Checking Steam achievements (steam achievements)' -ForegroundColor Cyan
        Send-Text 'steam achievements'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'term: ran steam' -Seconds 30)) {
            $failures += 'the shell never ran steam achievements'
        }
        Start-Sleep -Seconds 1

        Write-Host '==> Running Steamworks game diagnostic simulation (game test)' -ForegroundColor Cyan
        Send-Text 'game test'
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'game: diagnostic complete!' -Seconds 30)) {
            $failures += 'the game diagnostic never completed'
        }
        Start-Sleep -Seconds 1

        Write-Host '==> Launching Steam Spacewar from launcher (F3 -> game)' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 30)) {
            $failures += 'launcher did not reopen'
        }
        Start-Sleep -Milliseconds 800
        Send-Text 'game'
        Start-Sleep -Seconds 1
        Send-Keys @('ret')
        if (-not (Wait-For -Text 'compositor: started the interactive game client' -Seconds 30)) {
            $failures += 'compositor did not start game client'
        }
        Start-Sleep -Seconds 3

        $writer.WriteLine('quit')
        Start-Sleep -Milliseconds 800
    } finally {
        $client.Close()
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = if (Test-Path $Log) { (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '' } else { '' }

foreach ($expected in @(
        'game: Nexus Spacewar starting (Steam AppID 480)',
        'game: diagnostic complete!',
        'compositor: started the interactive game client'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }
if ($output.Contains('game: PANIC')) { $failures += 'the game panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Steamworks gaming subsystem tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}

