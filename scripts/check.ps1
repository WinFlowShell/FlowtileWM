param(
    [switch]$RequireUiHost
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$TargetDir = Join-Path $PSScriptRoot "..\tmp\target-script-check"

Write-Host "Running cargo fmt --check..."
cargo fmt --all --check

Write-Host "Running cargo clippy..."
cargo clippy --target-dir $TargetDir --workspace --all-targets -- -D warnings

Write-Host "Running cargo test..."
cargo test --target-dir $TargetDir --workspace

if ($RequireUiHost) {
    throw "UI Host project was removed from the current working line."
}

Write-Host "Check completed."
