<#
.SYNOPSIS
    Prune old owned test payloads while retaining logs, PNGs and the latest runs.
.DESCRIPTION
    Persistent disks, firmware settings, sources and runtime/toolchain caches
    are never selected. Use -WhatIf to inspect the selected files first.
#>
[CmdletBinding(SupportsShouldProcess)]
param([ValidateRange(0, 100)][int]$Keep = 2)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'maintenance.ps1')
Invoke-NexusCleanup -Keep $Keep -WhatIf:$WhatIfPreference
