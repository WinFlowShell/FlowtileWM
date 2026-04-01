param(
    [switch]$NoBuild,
    [switch]$NoStart,
    [switch]$NoStopExisting,
    [switch]$DryRunWatch,
    [switch]$NoDiagnosticsCollectors,
    [ValidateRange(500, 60000)]
    [int]$DiagnosticsSampleIntervalMs = 2000
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$scriptPath = Join-Path $PSScriptRoot "start-dev-runtime.ps1"
if (-not (Test-Path $scriptPath)) {
    throw "Expected helper script was not found: $scriptPath"
}

Write-Host "Starting hybrid native/surrogate smoke runtime with surrogate diagnostics enabled..."

& $scriptPath `
    -NoBuild:$NoBuild `
    -NoStart:$NoStart `
    -NoStopExisting:$NoStopExisting `
    -DryRunWatch:$DryRunWatch `
    -NoDiagnosticsCollectors:$NoDiagnosticsCollectors `
    -EnableSurrogateDiagnostics `
    -DiagnosticsSampleIntervalMs $DiagnosticsSampleIntervalMs
