<#
.SYNOPSIS
    EFI System Partition staging, shared by the build and test scripts.

.DESCRIPTION
    Dot-source this from a script that has built the bootloader and kernel:

        . (Join-Path $PSScriptRoot 'stage.ps1')

    Everything that writes into build/esp goes through here. It used to be done
    in two places with two different answers -- build.ps1 stripped the kernel,
    test-faults.ps1 copied it whole -- so whichever script ran last decided what
    the next QEMU run would actually boot, and "restoring the default kernel
    build" restored something the build had never produced.
#>

Set-StrictMode -Version Latest

# Copy a kernel into the ESP, without its debug information.
#
# Debug info is the overwhelming majority of the linked image and none of it is
# loadable, but the bootloader still has to read every byte off the ESP before
# it can parse the program headers -- which on a debug build means reading
# megabytes to load a few hundred kilobytes. The unstripped image stays in
# target/ for debuggers and symbolisation.
function Publish-EspKernel {
    param(
        [Parameter(Mandatory = $true)][string]$KernelElf,
        [Parameter(Mandatory = $true)][string]$EspDir
    )

    if (-not (Test-Path $KernelElf)) { throw "expected build output is missing: $KernelElf" }
    New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'nexus') | Out-Null

    $staged = Join-Path $EspDir 'nexus\kernel.elf'
    $sysroot = (& rustc +nightly --print sysroot).Trim()
    $objcopy = Join-Path $sysroot 'lib\rustlib\x86_64-pc-windows-msvc\bin\llvm-objcopy.exe'

    if (Test-Path $objcopy) {
        & $objcopy --strip-debug $KernelElf $staged
        if ($LASTEXITCODE -ne 0) { throw "llvm-objcopy failed (exit $LASTEXITCODE)" }
    } else {
        Write-Host '    llvm-objcopy not found; deploying an unstripped kernel' -ForegroundColor Yellow
        Copy-Item $KernelElf $staged -Force
    }

    return $staged
}

# Stage a complete bootable tree: the bootloader where firmware looks for it,
# and the kernel where the bootloader looks for it.
function Publish-Esp {
    param(
        [Parameter(Mandatory = $true)][string]$BootEfi,
        [Parameter(Mandatory = $true)][string]$KernelElf,
        [Parameter(Mandatory = $true)][string]$EspDir
    )

    if (-not (Test-Path $BootEfi)) { throw "expected build output is missing: $BootEfi" }
    New-Item -ItemType Directory -Force -Path (Join-Path $EspDir 'EFI\BOOT') | Out-Null
    Copy-Item $BootEfi (Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI') -Force

    $staged = Publish-EspKernel -KernelElf $KernelElf -EspDir $EspDir

    return [pscustomobject]@{
        BootEfi        = Join-Path $EspDir 'EFI\BOOT\BOOTX64.EFI'
        StagedKernel   = $staged
        BootSize       = [math]::Round((Get-Item $BootEfi).Length / 1KB, 1)
        KernelSize     = [math]::Round((Get-Item $staged).Length / 1KB, 1)
        UnstrippedSize = [math]::Round((Get-Item $KernelElf).Length / 1KB, 1)
    }
}
