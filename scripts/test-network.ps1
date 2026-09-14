<#
.SYNOPSIS
    Boots NexusOS and talks to it over TCP from the host.

.DESCRIPTION
    Everything else in this suite reads what the machine said about itself. This
    one asks it something.

    QEMU forwards a port on the host into the guest, so the connection below is
    an ordinary .NET TcpClient opening an ordinary socket. What answers is a TCP
    implementation written in this repository: a three-way handshake, sequence
    numbers, an acknowledgement for the request, a response, and a four-way
    close. Neither end knows anything about the other beyond what crosses the
    wire, which is the whole point -- a client that had to be taught about
    NexusOS would be proving nothing.

    The address, the lease and the ping are checked too, because those are what
    the connection stands on: a machine that could answer TCP without having
    been given an address would be a machine answering from an address it made
    up.

.PARAMETER Timeout
    How long to wait for the guest to be listening, in seconds.

.PARAMETER HostPort
    Which host port QEMU forwards into the guest.
#>
[CmdletBinding()]
param(
    [int]$Timeout = 300,
    [int]$HostPort = 18080
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot 'qemu.ps1')

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BuildDir = Join-Path $RepoRoot 'build'
$EspDir = Join-Path $BuildDir 'esp'
$SerialLog = Join-Path $BuildDir 'network-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}
if (Test-Path $SerialLog) { Remove-Item $SerialLog -Force }

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source

New-Item -ItemType Directory -Force -Path $BuildDir | Out-Null
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$Vars = Join-Path $BuildDir 'vars-network.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $Vars -Force

$QemuArgs = Get-NexusQemuArgs -BuildDir $BuildDir -EspDir $EspDir `
    -FirmwareCode $FirmwareCode -FirmwareVars $Vars -SerialLog $SerialLog `
    -HostHttpPort $HostPort -Headless

Write-Host "==> Booting NexusOS with port $HostPort forwarded to the guest's 80" -ForegroundColor Cyan
$process = Start-Process -FilePath $QemuExe.Source -ArgumentList $QemuArgs -PassThru -NoNewWindow

$failures = @()
$response = ''
try {
    # Wait for the guest to say it is listening. Waiting for a marker rather
    # than a number of seconds, because how long a boot takes belongs to the
    # host and not to the system under test.
    Write-Host '==> Waiting for the guest to listen' -ForegroundColor Cyan
    $listening = $false
    for ($waited = 0; $waited -lt $Timeout; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { throw "QEMU exited early with code $($process.ExitCode)" }
        if (Test-Path $SerialLog) {
            $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
            if ($sofar.Contains('listening on TCP port 80')) { $listening = $true; break }
        }
    }
    if (-not $listening) { throw 'the guest never started listening' }

    Write-Host "==> Connecting to 127.0.0.1:$HostPort" -ForegroundColor Cyan
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        # The connect itself is the handshake. If the guest's SYN-ACK is wrong
        # in any way -- a sequence number, a checksum, a flag -- this throws,
        # and nothing after it runs.
        $connect = $client.ConnectAsync('127.0.0.1', $HostPort)
        if (-not $connect.Wait(20000)) { throw 'the connection was not accepted' }

        $stream = $client.GetStream()
        $stream.ReadTimeout = 20000
        $request = [Text.Encoding]::ASCII.GetBytes("GET /nexus HTTP/1.1`r`nHost: nexusos`r`n`r`n")
        $stream.Write($request, 0, $request.Length)
        $stream.Flush()

        # Read until the guest closes, which is what its FIN means. A read that
        # returned as soon as it had something would not be checking that the
        # close works.
        $buffer = New-Object byte[] 4096
        $collected = New-Object System.IO.MemoryStream
        while ($true) {
            $read = $stream.Read($buffer, 0, $buffer.Length)
            if ($read -le 0) { break }
            $collected.Write($buffer, 0, $read)
            if ($collected.Length -gt 65536) { break }
        }
        $response = [Text.Encoding]::UTF8.GetString($collected.ToArray())
    } finally {
        $client.Close()
    }

    # Give the guest a moment to finish its close and report.
    for ($waited = 0; $waited -lt 30; $waited++) {
        Start-Sleep -Seconds 1
        if ($process.HasExited) { break }
        $sofar = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''
        if ($sofar.Contains('the connection closed cleanly')) { break }
    }
} finally {
    if (-not $process.HasExited) {
        try { $process.Kill() } catch { }
    }
    $process.WaitForExit(5000) | Out-Null
}

$output = (Get-Content $SerialLog -Raw -Encoding UTF8) -replace "`0", ''

# What the machine had to have done before it could answer at all.
foreach ($expected in @(
        'address 10.0.2.15/24 from 10.0.2.2',
        'ping 10.0.2.2: reply in',
        'listening on TCP port 80'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "the guest never reported: $expected"
    }
}

# And the connection itself.
if ($response -match '^HTTP/1\.1 200 OK') {
    Write-Host '    ok   the guest answered with a complete HTTP response' -ForegroundColor DarkGray
} else {
    $failures += 'the guest did not answer with an HTTP response'
}

# The response has to be *about this machine*. A fixed string would pass a
# check that only looked for "NexusOS"; the address and the uptime could not
# have been written in advance.
if ($response -match 'address     : 10\.0\.2\.15/24') {
    Write-Host '    ok   and it named the address it was leased' -ForegroundColor DarkGray
} else {
    $failures += 'the response did not carry the leased address'
}
if ($response -match 'uptime      : (\d+)\.(\d+) s' -and [int]$Matches[1] -ge 0) {
    Write-Host "    ok   and its uptime at the moment it answered ($($Matches[1]).$($Matches[2]) s)" -ForegroundColor DarkGray
} else {
    $failures += 'the response did not carry an uptime'
}
# It read the request rather than ignoring it.
if ($response -match 'you asked   : GET /nexus HTTP/1\.1') {
    Write-Host '    ok   and it repeated back what it was asked' -ForegroundColor DarkGray
} else {
    $failures += 'the guest did not read the request it was sent'
}

# Content-Length has to match what actually arrived, which is what says the
# whole response crossed and nothing was lost or doubled.
if ($response -match 'Content-Length: (\d+)') {
    $declared = [int]$Matches[1]
    $split = $response.IndexOf("`r`n`r`n")
    $body = $response.Substring($split + 4)
    if ($body.Length -eq $declared) {
        Write-Host "    ok   the body is the $declared bytes it said it would be" -ForegroundColor DarkGray
    } else {
        $failures += "the body is $($body.Length) bytes but Content-Length said $declared"
    }
} else {
    $failures += 'the response had no Content-Length'
}

# The guest's own account of the exchange.
if ($output.Contains('a request arrived: GET /nexus')) {
    Write-Host '    ok   the guest logged the request it read' -ForegroundColor DarkGray
} else {
    $failures += 'the guest never logged a request'
}
if ($output.Contains('the connection closed cleanly')) {
    Write-Host '    ok   and the connection closed cleanly at its end too' -ForegroundColor DarkGray
} else {
    $failures += 'the connection did not close cleanly at the guest'
}

if ($output.Contains('EXCEPTION')) { $failures += 'an exception was reported' }
if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Network tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Serial log: $SerialLog"
    exit 1
}
