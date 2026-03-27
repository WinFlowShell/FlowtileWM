param(
    [switch]$RequireVisualCompanion
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

if ($RequireVisualCompanion) {
    throw "A separate visual companion process is not part of the current working line."
}

Write-Host "Check completed."
