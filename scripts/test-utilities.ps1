<#
.SYNOPSIS
    Tests the new desktop power tools: Task Manager, Calculator, and Device Manager on NexusOS.
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
$Log = Join-Path $BuildDir 'utilities-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-utilities.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 45000 -Maximum 45999
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
                    '/' { 'slash' }
                    '-' { 'minus' }
                    default { $letter }
                }
                Send-Keys @($key) -Pause 105
            }
        }

        # 1. Test Task Manager
        Write-Host '==> Launching Task Manager from launcher (F3 -> taskmgr)' -ForegroundColor Cyan
        Send-Keys @('f3')
        if (-not (Wait-For -Text 'launch: a window for starting things by name' -Seconds 60)) {
            throw 'the launcher never started'
        }
        Start-Sleep -Milliseconds 800
        Send-Text 'task'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the task manager and performance monitor' -Seconds 60)) {
            $failures += 'compositor did not start taskmgr'
        } else {
            Write-Host '    ok compositor started taskmgr' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2

        # Switch tabs in Task Manager: Processes (2), Specs (3), Performance (1)
        Send-Keys @('2')
        Start-Sleep -Milliseconds 300
        Send-Keys @('down', 'down')
        Start-Sleep -Milliseconds 300
        Send-Keys @('3')
        Start-Sleep -Milliseconds 300
        Send-Keys @('1')
        Start-Sleep -Milliseconds 300

        # 2. Test Calculator
        Write-Host '==> Launching Calculator from launcher (F3 -> calc)' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Milliseconds 800
        Send-Text 'calc'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the graphical calculator' -Seconds 60)) {
            $failures += 'compositor did not start calc'
        } else {
            Write-Host '    ok compositor started calc' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2

        # Send math calculations: 4 * 16 =
        Send-Keys @('4', 'asterisk', '1', '6', 'equal')
        Start-Sleep -Milliseconds 500
        # Switch to programmer mode [p]
        Send-Keys @('p')
        Start-Sleep -Milliseconds 500

        # 3. Test Device Manager
        Write-Host '==> Launching Device Manager from launcher (F3 -> devmgr)' -ForegroundColor Cyan
        Send-Keys @('f3')
        Start-Sleep -Milliseconds 800
        Send-Text 'dev'
        Start-Sleep -Milliseconds 500
        Send-Keys @('ret')

        if (-not (Wait-For -Text 'compositor: started the hardware device manager' -Seconds 60)) {
            $failures += 'compositor did not start devmgr'
        } else {
            Write-Host '    ok compositor started devmgr' -ForegroundColor Green
        }
        Start-Sleep -Seconds 2
        Send-Keys @('down', 'down', 'up')
        Start-Sleep -Seconds 1

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

$content = if (Test-Path $Log) { (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", '' } else { '' }

if ($content.Contains('KERNEL PANIC')) {
    $failures += 'kernel panic observed'
}
if ($content.Contains('taskmgr: PANIC')) {
    $failures += 'taskmgr crashed'
}
if ($content.Contains('calc: PANIC')) {
    $failures += 'calc crashed'
}
if ($content.Contains('devmgr: PANIC')) {
    $failures += 'devmgr crashed'
}

if ($failures.Count -gt 0) {
    Write-Host "`nTest FAILED with $($failures.Count) error(s):" -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" -ForegroundColor Red }
    exit 1
}

Write-Host "`nAll Task Manager, Calculator, and Device Manager tests PASSED!" -ForegroundColor Green
exit 0

