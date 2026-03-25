Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

Write-Host "Running core-daemon smoke test..."
cargo run -p flowtile-core-daemon -- watch --poll-only --iterations 1

Write-Host "Running CLI smoke test..."
cargo run -p flowtile-cli -- status

Write-Warning "UI Host smoke test is intentionally skipped until WinUI 3 / Windows App SDK bootstrap is completed."

Write-Host "System smoke checks completed."
