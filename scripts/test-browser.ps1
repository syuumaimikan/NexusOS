<#
.SYNOPSIS
    Opens a browser window from the desktop and checks that it fetched a page.

.DESCRIPTION
    The whole path, end to end, with nothing outside the machine involved:

      the desktop is clicked -> the compositor starts the browser and lends it
      the network -> the browser asks the kernel's network service to open a
      connection -> the client half of the TCP stack sends a SYN to this
      machine's own address -> the packet is looped back into the receive path
      -> the server half answers the handshake -> the browser sends an HTTP
      request -> the server builds a page about the machine -> the browser reads
      it, parses it and lays it out.

    Every one of those is a piece somebody could have got wrong in a way that
    only shows up when the next piece uses it, which is why this is one test and
    not nine.

    It needs a machine that has been set up, because there is no desktop to
    click on until somebody has answered the wizard.

.PARAMETER Timeout
    How long to wait for each stage, in seconds.
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
$Log = Join-Path $BuildDir 'browser-test.log'

if (-not (Test-Path (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'))) {
    throw 'no staged ESP; run build.ps1 first'
}

$QemuExe = Get-Command qemu-system-x86_64 -ErrorAction Stop
$QemuDir = Split-Path -Parent $QemuExe.Source
$FirmwareCode = Join-Path $BuildDir 'edk2-x86_64-code.fd'
if (-not (Test-Path $FirmwareCode)) {
    Copy-Item (Join-Path $QemuDir 'share\edk2-x86_64-code.fd') $FirmwareCode -Force
}
$FirmwareVars = Join-Path $BuildDir 'vars-browser.fd'
Copy-Item (Join-Path $QemuDir 'share\edk2-i386-vars.fd') $FirmwareVars -Force

if (Test-Path $Log) { Remove-Item $Log -Force }

$MonitorPort = Get-Random -Minimum 30000 -Maximum 31999
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
    # And for an address, because a browser with no address has nowhere to go.
    if (-not (Wait-For -Text 'address 10.0.2.15/24' -Seconds 60)) {
        throw 'the machine never got an address'
    }

    $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $MonitorPort)
    try {
        $writer = New-Object System.IO.StreamWriter($client.GetStream())
        $writer.AutoFlush = $true
        Start-Sleep -Milliseconds 800

        function Move-Pointer {
            param([int]$Dx, [int]$Dy, [int]$Steps, [int]$Pause = 35)
            foreach ($step in 1..$Steps) {
                $writer.WriteLine("mouse_move $Dx $Dy")
                Start-Sleep -Milliseconds $Pause
            }
        }

        # The button that opens a page is in the strip along the bottom, just
        # past the one that starts a program. Driven into the corner first,
        # where the pointer clamps, so nothing here has to know where it was.
        Write-Host '==> Pressing the button that opens a page' -ForegroundColor Cyan
        Move-Pointer -Dx -60 -Dy 60 -Steps 40 -Pause 20
        Move-Pointer -Dx 0 -Dy -3 -Steps 5      # into the middle of the buttons
        Move-Pointer -Dx 20 -Dy 0 -Steps 5      # x about 100: the Web button
        $writer.WriteLine('mouse_button 1')
        Start-Sleep -Milliseconds 250
        $writer.WriteLine('mouse_button 0')

        if (-not (Wait-For -Text 'compositor: started a browser' -Seconds 60)) {
            $failures += 'the desktop never started a browser'
        }
        if (-not (Wait-For -Text 'browse: showed' -Seconds 120)) {
            $failures += 'the browser never showed a page'
        }

        # And save it. The page is in the window now, which means the browser is
        # holding the bytes that arrived rather than the text it laid out -- F2
        # writes those bytes into the one folder it was lent.
        Write-Host '==> Saving the page' -ForegroundColor Cyan
        $writer.WriteLine('sendkey f2')
        if (-not (Wait-For -Text 'browse: saved' -Seconds 60)) {
            $failures += 'the browser never saved the page'
        }

        # The figures below come from the kernel's monitor thread, which reports
        # every five seconds. This used to sleep a fixed six seconds and hope,
        # and it was a coin toss: a page that finished just after a report left
        # the next one nearly five seconds away, and the log was cut before it
        # arrived. That is not a browser failing -- the fetch above had already
        # passed -- but it failed the run, which is worse than useless, because
        # a suite that cries wolf is a suite people stop reading.
        #
        # So wait for the lines themselves. If they never come, that *is* a
        # finding: the stack did the work without counting it.
        if (-not (Wait-For -Text '[mon ] loopback ' -Seconds 30)) {
            $failures += 'the kernel never reported the loopback figures'
        }
        if (-not (Wait-For -Text '[mon ] client ' -Seconds 30)) {
            $failures += 'the kernel never reported the client figures'
        }

        # And stop the machine through the monitor rather than killing it, so
        # the serial file is closed with everything in it. Killing QEMU has cost
        # this repository at least two failures that looked like product bugs
        # and were the last few lines of a log going missing.
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

$output = (Get-Content $Log -Raw -Encoding UTF8) -replace "`0", ''

foreach ($expected in @(
        'compositor: started a browser, and lent it the network',
        'browse: a window that can fetch a page',
        'a program opened a connection to 10.0.2.15:80',
        'browse: showed http://10.0.2.15/ (200)',
        # Saved, with a name worked out from the URL and a byte count. The name
        # matters: the path is `/`, which has no last component, so this is the
        # front-page case rather than a name taken from the server.
        'browse: saved index'
    )) {
    if ($output.Contains($expected)) {
        Write-Host "    ok   $expected" -ForegroundColor DarkGray
    } else {
        $failures += "never reported: $expected"
    }
}

# The page has to have been parsed into something, not merely fetched. A
# browser that showed nought blocks fetched bytes and understood none of them.
if ($output -match 'browse: showed [^,]+, (\d+) blocks, (\d+) links') {
    $blocks = [int]$Matches[1]
    if ($blocks -lt 2) {
        $failures += "the page came out as $blocks blocks, which is not a page"
    } else {
        Write-Host "    ok   the page became $blocks blocks" -ForegroundColor DarkGray
    }
} else {
    $failures += 'the browser never said what it made of the page'
}

# And the packets have to have gone the way this test claims they did.
if ($output -match 'loopback (\d+) packets this machine sent to itself') {
    Write-Host "    ok   $($Matches[1]) packets went round the loop" -ForegroundColor DarkGray
} else {
    $failures += 'nothing went through the loopback path'
}
if ($output -match 'client (\d+) connections opened, (\d+) bytes out, (\d+) in') {
    if ([int]$Matches[3] -lt 100) {
        $failures += "only $($Matches[3]) bytes came back, which is not a page"
    } else {
        Write-Host "    ok   $($Matches[2]) bytes out, $($Matches[3]) in" -ForegroundColor DarkGray
    }
} else {
    $failures += 'the stack never reported a connection of its own'
}
if ($output.Contains('KERNEL PANIC')) { $failures += 'the kernel panicked' }
if ($output.Contains('browse: PANIC')) { $failures += 'the browser panicked' }

# And the check that is outside the guest: the bytes are in the host's copy of
# the disk. A browser that reported a save and wrote nothing passes every check
# above and fails this one.
$checks = if (Test-Path variable:checks) { $checks } else { 0 }
$image = Join-Path $BuildDir 'nexus-disk.img'
# The name the browser said it used, this run. Not a fixed one: saving the same
# page twice numbers the second, so a test that looked for `index.html` would
# find the *previous* run's file and pass while this run had failed.
$named = [regex]::Match($output, 'browse: saved (\S+?), (\d+) bytes')
if (-not $named.Success) {
    $failures += 'the log never said what was saved'
    $savedAs = 'index.html'
} else {
    $savedAs = $named.Groups[1].Value
    Write-Host "    .... looking for $savedAs in the host's image" -ForegroundColor DarkGray
}
$needle = [System.Text.Encoding]::ASCII.GetBytes($savedAs)
$found = $false
$stream = [System.IO.File]::OpenRead($image)
try {
    # From the start of the NexusFS partition; there is no point reading the
    # FAT32 in front of it.
    $stream.Position = 526336L * 512
    $overlap = $needle.Length - 1
    $chunk = 8MB
    $buffer = New-Object byte[] ($chunk + $overlap)
    $held = 0
    while (-not $found) {
        $got = $stream.Read($buffer, $held, $chunk)
        if ($got -le 0) { break }
        $usable = $held + $got
        $last = $usable - $needle.Length
        $index = 0
        while ($index -le $last) {
            $index = [Array]::IndexOf($buffer, $needle[0], $index, $last - $index + 1)
            if ($index -lt 0) { break }
            $match = $true
            for ($offset = 1; $offset -lt $needle.Length; $offset++) {
                if ($buffer[$index + $offset] -ne $needle[$offset]) { $match = $false; break }
            }
            if ($match) { $found = $true; break }
            $index++
        }
        $held = [math]::Min($overlap, $usable)
        [Array]::Copy($buffer, $usable - $held, $buffer, 0, $held)
    }
} finally {
    $stream.Close()
}
if ($found) {
    Write-Host "    ok   the saved file is in the host's disk image" -ForegroundColor DarkGray
} else {
    $failures += "what the browser saved is not in $image"
    Write-Host "    FAIL the saved file is not in the host's image" -ForegroundColor Red
}

Write-Host ''
if ($failures.Count -eq 0) {
    Write-Host 'Browser tests passed.' -ForegroundColor Green
    exit 0
} else {
    foreach ($failure in $failures) {
        Write-Host "    FAIL $failure" -ForegroundColor Red
    }
    Write-Host "Log: $Log"
    exit 1
}
