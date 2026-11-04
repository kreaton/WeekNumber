<#
.SYNOPSIS
    Builds WeekNumber.

.DESCRIPTION
    By default the C runtime is linked statically (see .cargo/config.toml), so the exe
    runs on any Windows PC without vcruntime140.dll installed.

    Use -DynamicCrt to link the C runtime dynamically instead. Note that switching
    between the two rebuilds all dependencies.

.EXAMPLE
    .\build.ps1                       # release, static C runtime
.EXAMPLE
    .\build.ps1 -DynamicCrt           # release, needs vcruntime140.dll at run time
.EXAMPLE
    .\build.ps1 -DebugBuild           # debug build in target\debug
#>
param(
    # Link the C runtime dynamically (requires vcruntime140.dll on the target PC).
    [switch]$DynamicCrt,
    # Build the debug profile instead of release.
    [switch]$DebugBuild
)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$cargoArgs = @('build')
if (-not $DebugBuild) { $cargoArgs += '--release' }

# RUSTFLAGS overrides the rustflags in .cargo/config.toml. Remember any existing value
# so it can be restored afterwards.
$previous = $env:RUSTFLAGS
try {
    if ($DynamicCrt) {
        $env:RUSTFLAGS = '-C target-feature=-crt-static'
    }
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
    if ($null -eq $previous) {
        Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    } else {
        $env:RUSTFLAGS = $previous
    }
}

$buildProfile = if ($DebugBuild) { 'debug' } else { 'release' }
$crt = if ($DynamicCrt) { 'dynamic' } else { 'static' }
Write-Host "Built target\$buildProfile\weeknumber.exe ($crt C runtime)" -ForegroundColor Green
