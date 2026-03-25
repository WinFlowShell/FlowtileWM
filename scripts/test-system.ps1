Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$TargetDir = Join-Path $PSScriptRoot "..\tmp\target-script-system"
$DaemonExe = Join-Path $TargetDir "debug\flowtile-core-daemon.exe"
$CliExe = Join-Path $TargetDir "debug\flowtile-cli.exe"
$daemonProcess = $null
$existingDaemons = @(Get-Process flowtile-core-daemon -ErrorAction SilentlyContinue)

if ($existingDaemons.Count -gt 0) {
    throw "System smoke test requires no running flowtile-core-daemon process, otherwise CLI status can attach to the wrong daemon."
}

Write-Host "Building smoke-test binaries..."
cargo build --target-dir $TargetDir -p flowtile-core-daemon -p flowtile-cli

try {
    Write-Host "Starting temporary daemon for smoke test..."
    $daemonProcess = Start-Process `
        -FilePath $DaemonExe `
        -ArgumentList @("watch", "--poll-only") `
        -PassThru `
        -WindowStyle Hidden
    Start-Sleep -Milliseconds 750

    if ($daemonProcess.HasExited) {
        throw "Smoke-test daemon exited before CLI status check."
    }

    Write-Host "Running CLI smoke test against temporary daemon..."
    & $CliExe status
}
finally {
    if ($null -ne $daemonProcess -and -not $daemonProcess.HasExited) {
        Stop-Process -Id $daemonProcess.Id -Force
        $daemonProcess.WaitForExit()
    }
}

Write-Host "System smoke checks completed."
