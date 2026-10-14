<#
.SYNOPSIS
    Verify native npkg installation, Linux execution, rollback and persistence.
.DESCRIPTION
    Uses an owned disposable disk; never changes the user's persistent image.
    The Windows fixture is a real PE payload stored with a signed manifest.
    This does not claim Win32 execution or physical GPU acceleration.
#>
[CmdletBinding()]
param([int]$Timeout = 180)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
. (Join-Path $PSScriptRoot 'maintenance.ps1')
. (Join-Path $PSScriptRoot 'qemu.ps1')
. (Join-Path $PSScriptRoot 'private-machine.ps1')
. (Join-Path $PSScriptRoot 'capture.ps1')
$Repo = Split-Path -Parent $PSScriptRoot
$Run = Join-Path $Repo ('build/package-test-' + [guid]::NewGuid().ToString('N'))
Register-NexusDisposableRun -Directory $Run
$Programs = Join-Path $Run 'programs'
Copy-Item -LiteralPath (Join-Path $Repo 'build/programs') -Destination $Programs -Recurse
Copy-Item -LiteralPath (Join-Path $Repo 'build/esp') -Destination (Join-Path $Run 'esp') -Recurse
foreach ($Name in @('nexus-usb.img', 'nexus-usb-fs.img')) {
    $Source = Join-Path $Repo "build/$Name"
    if (Test-Path -LiteralPath $Source) { Copy-Item -LiteralPath $Source -Destination (Join-Path $Run $Name) }
}
Push-Location $Repo
try {
    & cargo build --offline -p nexus-pack
    if ($LASTEXITCODE -ne 0) { throw 'packer build failed' }
} finally { Pop-Location }
$Pack = Join-Path $Repo 'target/debug/nexus-pack.exe'
$Key = Join-Path $Repo 'keys/development.key'
$Utf8 = New-Object System.Text.UTF8Encoding $false
function Write-Fixture([string]$Name, [string]$Text) {
    $Path = Join-Path $Run $Name
    [IO.File]::WriteAllText($Path, $Text, $Utf8)
    return $Path
}
function Pack-Fixture([string]$File, [string]$Name, [string[]]$Files) {
    & $Pack (Join-Path $Programs $File) $Key $Name '1.0.0' @Files
    if ($LASTEXITCODE -ne 0) { throw "packing $File failed" }
}
# A normal signed first-boot package supplies the test's manual-update policy.
# It keeps the real demo data, so init's independent readback still checks it.
$Policy = Write-Fixture 'policy.txt' "system.configured = yes`nsystem.language = en-US`nsystem.updates = ask`n"
Pack-Fixture 'demo.nex' 'demo' @(
    "demo/hello.txt=$(Join-Path $Repo 'build/package/hello.txt')",
    "demo/notes.txt=$(Join-Path $Repo 'build/package/notes.txt')",
    "system/settings.txt=$Policy"
)
$LinuxManifest = Write-Fixture 'linux.app' "format=1`nplatform=linux`narch=x86_64`nentry=linux/usr/bin/pkghello`n"
Pack-Fixture 'linux.nex' 'linuxdemo' @("--manifest=$LinuxManifest", "linux/usr/bin/pkghello=$(Join-Path $Repo 'build/programs/hello.lx')")
$WindowsManifest = Write-Fixture 'windows.app' "format=1`nplatform=windows`narch=x86_64`nentry=APPS/windowsdemo/bin/pack.EXE`n"
Pack-Fixture 'windows.nex' 'windowsdemo' @("--manifest=$WindowsManifest", "APPS/windowsdemo/bin/pack.EXE=$Pack")
$Changed = Write-Fixture 'changed.txt' 'this would destroy the executable if rollback did not work'
Pack-Fixture 'roll.nex' 'rollbackdemo' @("linux/usr/bin/pkghello=$Changed", "collision/child=$Changed")
$Bad = [IO.File]::ReadAllBytes((Join-Path $Programs 'linux.nex'))
$Bad[$Bad.Length - 1] = $Bad[$Bad.Length - 1] -bxor 1
[IO.File]::WriteAllBytes((Join-Path $Programs 'forged.nex'), $Bad)
$Disk = Join-Path $Run 'nexus-disk.img'
& (Join-Path $PSScriptRoot 'make-disk.ps1') -Output $Disk -ProgramDir $Programs -SizeMiB 512 -FatMiB 256
function Get-NexusDiskSizes { return @{ SizeMiB = 512; FatMiB = 256 } }
$Qemu = (Get-Command qemu-system-x86_64).Source
$Share = Join-Path (Split-Path -Parent $Qemu) 'share'
$Code = Join-Path $Run 'code.fd'
$Vars = Join-Path $Run 'vars.fd'
Copy-Item -LiteralPath (Join-Path $Share 'edk2-x86_64-code.fd') -Destination $Code
Copy-Item -LiteralPath (Join-Path $Share 'edk2-i386-vars.fd') -Destination $Vars
$Guest = $null
$Writer = $null
$Connection = $null
$Serial = ''
function Read-Log {
    if (Test-Path -LiteralPath $Serial) { return ([string](Get-Content -LiteralPath $Serial -Raw -Encoding UTF8)) -replace "`0", '' }
    return ''
}
function Wait-Marker([string]$Marker, [int]$Offset = 0) {
    $Deadline = [DateTime]::UtcNow.AddSeconds($Timeout)
    while ([DateTime]::UtcNow -lt $Deadline -and -not $Guest.HasExited) {
        $Log = Read-Log
        if ($Log -match '(?m)^ EXCEPTION [0-9]+:|KERNEL PANIC|user fault|npkg: PANIC|init: FAILED|update: FAILED') {
            throw "guest failed: $Serial"
        }
        if ($Log.Length -ge $Offset -and $Log.Substring($Offset).Contains($Marker)) { return }
        Start-Sleep -Milliseconds 250
    }
    throw "missing '$Marker': $Serial"
}
function Send-Key([string]$Key) {
    $Writer.WriteLine("sendkey $Key")
    Start-Sleep -Milliseconds 150
}
function Send-Text([string]$Text) {
    foreach ($Letter in $Text.ToCharArray()) {
        $KeyName = switch ($Letter) {
            ' ' { 'spc' }
            '.' { 'dot' }
            '/' { 'slash' }
            '-' { 'minus' }
            ':' { 'shift-semicolon' }
            default {
                if ([char]::IsUpper($Letter)) { 'shift-' + [char]::ToLowerInvariant($Letter) }
                else { [string]$Letter }
            }
        }
        Send-Key $KeyName
    }
}
function Command([string]$Text, [string]$Marker) {
    $Offset = (Read-Log).Length
    Send-Text $Text
    Send-Key 'ret'
    Wait-Marker $Marker $Offset
    Wait-Marker ("term: ran " + $Text.Split(' ')[0]) $Offset
    if ($Text.StartsWith('npkg ')) { Wait-Marker 'term: BIN/NPKG.ELF wrote' $Offset }
    Write-Host "PASS: $Text" -ForegroundColor Green
}
function Boot([string]$Name) {
    $script:Serial = Join-Path $Run "$Name-serial.log"
    $Port = Get-FreeMonitorPort -From 41000 -To 41999
    $Args = Get-NexusQemuArgs -BuildDir $Run -EspDir (Join-Path $Run 'esp') -FirmwareCode $Code -FirmwareVars $Vars `
        -SerialLog $Serial -MonitorPort $Port -HostHttpPort 0 -Headless
    $script:Guest = Start-NexusQemu -Executable $Qemu -Arguments $Args -ErrorLog (Join-Path $Run "$Name-qemu-error.log")
    Wait-Marker 'init: the machine checked itself for updates'
    $script:Connection = [Net.Sockets.TcpClient]::new('127.0.0.1', $Port)
    $script:Writer = [IO.StreamWriter]::new($Connection.GetStream())
    $Writer.AutoFlush = $true
    Start-Sleep -Milliseconds 800
    if ((Read-Log).Contains('setup: this machine has not been set up; asking')) {
        Send-Key 'ret'
        Send-Key 'ret'
        Send-Text 'packagecheck'
        Send-Key 'ret'
        Send-Text 'testpass'
        Send-Key 'ret'
        Send-Text 'testpass'
        Send-Key 'ret'
        Send-Key 'ret'
        Wait-Marker 'setup: wrote settings'
        Send-Key 'ret'
    }
    Wait-Marker 'shell: took the strip'
    Send-Key 'meta_l-ret'
    Wait-Marker 'compositor: started a terminal'
    Start-Sleep -Milliseconds 1500
}
function Stop-Guest {
    if ($Writer) { try { $Writer.WriteLine('quit') } catch {} }
    if ($Connection) { $Connection.Dispose(); $script:Connection = $null; $script:Writer = $null }
    if ($Guest) {
        if (-not $Guest.HasExited -and -not $Guest.WaitForExit(5000)) { $Guest.Kill(); $Guest.WaitForExit(5000) | Out-Null }
        $Guest.Dispose(); $script:Guest = $null
    }
}
try {
    Boot 'install'
    Command 'npkg check PKG/FORGED.NEX' 'npkg: refused: the package''s signature is not from a trusted key'
    Command 'npkg info PKG/LINUX.NEX' 'npkg: platform=linux arch=x86_64 entry=linux/usr/bin/pkghello'
    Command 'cd PKG' 'term: ran cd'
    Command 'npkg install LINUX.NEX' 'npkg: installed and recorded'
    Command 'cd ..' 'term: ran cd'
    Command 'run linux:/usr/bin/pkghello' 'a program built for Linux, running on NexusOS'
    Command 'npkg install PKG/WINDOWS.NEX' 'npkg: Windows payload stored; Win32 execution is not available yet.'
    Command 'write collision protected' 'term: ran write'
    Command 'npkg install PKG/ROLL.NEX' 'npkg: FAILED: collision: parent is not a directory; rolled back 1 changes'
    Command 'run linux:/usr/bin/pkghello' 'a program built for Linux, running on NexusOS'
    Command 'write .INSTALL.LOCK busy' 'term: ran write'
    Command 'npkg install PKG/LINUX.NEX' 'npkg: FAILED: cannot acquire installation lock'
    Command 'rm .INSTALL.LOCK' 'term: ran rm'
    Command 'npkg list' 'linuxdemo = 1.0.0'
    $Log = Read-Log
    if ($Log.Contains('rollbackdemo = 1.0.0')) { throw 'failed package was recorded' }
    if (-not $Log.Contains('windowsdemo = 1.0.0')) { throw 'Windows payload was not recorded' }
    Start-Sleep -Seconds 2
    Invoke-Screendump -Writer $Writer -Path (Join-Path $Run 'manager.ppm')
    Convert-PpmToPng -PpmPath (Join-Path $Run 'manager.ppm') -PngPath (Join-Path $Run 'manager.png') | Out-Null
    Stop-Guest
    Boot 'restart'
    Command 'npkg list' 'linuxdemo = 1.0.0'
    Command 'run linux:/usr/bin/pkghello' 'a program built for Linux, running on NexusOS'
    Command 'npkg info PKG/WINDOWS.NEX' 'npkg: platform=windows arch=x86_64 entry=APPS/windowsdemo/bin/pack.EXE'
    Write-Host "PASS: signed native package manager, genuine installed Linux execution, rollback, lock refusal and restart persistence; $Run" -ForegroundColor Green
} finally { Stop-Guest }
