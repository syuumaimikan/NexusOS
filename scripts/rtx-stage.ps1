# Native bootstrap material only; do not label this as a running GPU driver.
function Publish-NvidiaBootstrap {
    param([Parameter(Mandatory = $true)][string]$ProgramDir)
    $RtxRepo = Split-Path -Parent $PSScriptRoot
    & python (Join-Path $RtxRepo 'tools/nexus-runtime/prepare-nvidia.py')
    if ($LASTEXITCODE -ne 0) { throw 'Pinned NVIDIA bootstrap extraction failed' }
    Push-Location $RtxRepo
    try {
        & cargo run --offline -p nexus-nvidia --example inspect -- (Join-Path $RtxRepo 'build/nvidia/bundle')
        if ($LASTEXITCODE -ne 0) { throw 'NVIDIA bootstrap format/release verification failed' }
    } finally { Pop-Location }
    $Destination = Join-Path $ProgramDir 'NVFW'
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($File in @('GSPBOOT.BIN','FMC.BIN','FWINFO.TXT','LICENSE.TXT')) {
        Copy-Item -LiteralPath (Join-Path $RtxRepo "build/nvidia/bundle/$File") -Destination (Join-Path $Destination $File) -Force
    }
}

function Publish-NvidiaGsp {
    param([Parameter(Mandatory = $true)][string]$ProgramDir)
    $GspRepo = Split-Path -Parent $PSScriptRoot
    & python (Join-Path $GspRepo 'tools/nexus-runtime/fetch-gsp.py')
    if ($LASTEXITCODE -ne 0) { throw 'Pinned NVIDIA GSP package extraction failed (Python 3.14 required)' }
    Push-Location $GspRepo
    try {
        & cargo run --offline -p nexus-nvidia --example inspect_gsp -- (Join-Path $GspRepo 'build/nvidia/gsp-bundle')
        if ($LASTEXITCODE -ne 0) { throw 'NVIDIA GSP container/release verification failed' }
    } finally { Pop-Location }
    $Destination = Join-Path $ProgramDir 'NVFW'
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    foreach ($File in @('GSPRM.BIN','UCODES.BIN','GSPINFO.TXT','NVLIC.TXT')) {
        Copy-Item -LiteralPath (Join-Path $GspRepo "build/nvidia/gsp-bundle/$File") -Destination (Join-Path $Destination $File) -Force
    }
}
