param(
    [switch]$NoBuild,
    [switch]$NoStart,
    [switch]$NoStopExisting,
    [switch]$DryRunWatch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$daemonTargetDir = Join-Path $repoRoot "tmp\target-touchpad-dev"
$helperBuildDir = Join-Path $repoRoot "tmp\touchpad-helper-build"
$runDir = Join-Path $repoRoot "tmp\touchpad-dev-run"
$daemonExe = Join-Path $daemonTargetDir "debug\flowtile-core-daemon.exe"
$helperDll = Join-Path $helperBuildDir "flowtile-touchpad-helper.dll"
$daemonStdout = Join-Path $runDir "daemon.stdout.log"
$daemonRuntimeLog = Join-Path $runDir "daemon.runtime.log"
$helperStdout = Join-Path $runDir "helper.stdout.log"
$helperStderr = Join-Path $runDir "helper.stderr.log"
$helperTouchpadLog = Join-Path $runDir "touchpad-helper.log"

function Ensure-Directory {
    param([string]$Path)

    if (-not (Test-Path $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Stop-ExistingTouchpadDevProcesses {
    $knownExecutables = @(
        (Join-Path $daemonTargetDir "debug\flowtile-core-daemon.exe"),
        (Join-Path $repoRoot "target\debug\flowtile-core-daemon.exe"),
        (Join-Path $repoRoot "apps\touchpad-helper\bin\Debug\net8.0-windows\flowtile-touchpad-helper.exe")
    )

    $candidateProcesses = Get-CimInstance Win32_Process |
        Where-Object {
            $_.Name -in @("flowtile-core-daemon.exe", "flowtile-touchpad-helper.exe", "dotnet.exe")
        }

    foreach ($process in $candidateProcesses) {
        $commandLine = $process.CommandLine
        $executablePath = $process.ExecutablePath

        $isHelperDotnet = $process.Name -eq "dotnet.exe" -and
            $null -ne $commandLine -and
            $commandLine.Contains("Flowtile.TouchpadHelper.csproj")

        $isKnownExecutable = $null -ne $executablePath -and $knownExecutables -contains $executablePath

        if (-not $isHelperDotnet -and -not $isKnownExecutable) {
            continue
        }

        Write-Host "Stopping existing process $($process.Name) (PID $($process.ProcessId))..."
        Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop
    }
}

Ensure-Directory $runDir

if (-not $NoStopExisting) {
    Stop-ExistingTouchpadDevProcesses
}

if (-not $NoBuild) {
    Write-Host "Building flowtile-core-daemon into $daemonTargetDir..."
    cargo build --target-dir $daemonTargetDir -p flowtile-core-daemon

    Write-Host "Building touchpad-helper into $helperBuildDir..."
    dotnet build (Join-Path $repoRoot "apps\touchpad-helper\Flowtile.TouchpadHelper.csproj") -o $helperBuildDir
}

if (-not (Test-Path $daemonExe)) {
    throw "Daemon executable was not found: $daemonExe"
}

if (-not (Test-Path $helperDll)) {
    throw "Touchpad helper build output was not found: $helperDll"
}

Write-Host ""
Write-Host "Touchpad dev entrypoint is prepared."
Write-Host "Daemon executable: $daemonExe"
Write-Host "Helper DLL: $helperDll"
Write-Host "Run logs: $runDir"
Write-Host ""
Write-Host "Manual daemon run:"
Write-Host "  & `"$daemonExe`" watch"
Write-Host ""
Write-Host "Manual helper run:"
Write-Host "  dotnet `"$helperDll`""

if ($NoStart) {
    return
}

Remove-Item $daemonStdout, $daemonRuntimeLog, $helperStdout, $helperStderr, $helperTouchpadLog -ErrorAction SilentlyContinue

[string[]]$daemonArguments = if ($DryRunWatch) {
    @("watch", "--dry-run", "--poll-only")
} else {
    @("watch")
}

Write-Host ""
Write-Host "Starting touchpad-helper in background without a new console window..."
$helper = Start-Process `
    -FilePath "dotnet" `
    -ArgumentList @($helperDll) `
    -WorkingDirectory $repoRoot `
    -NoNewWindow `
    -RedirectStandardOutput $helperStdout `
    -RedirectStandardError $helperStderr `
    -Environment @{ FLOWTILE_TOUCHPAD_HELPER_LOG_PATH = $helperTouchpadLog } `
    -PassThru

Start-Sleep -Seconds 2
if ($helper.HasExited) {
    Write-Host ""
    Write-Host "Helper stderr:"
    if (Test-Path $helperStderr) {
        Get-Content $helperStderr
    }
    throw "touchpad-helper exited during startup with code $($helper.ExitCode)."
}

Write-Host ""
Write-Host "Touchpad helper is running."
Write-Host "Helper PID: $($helper.Id)"
Write-Host "Helper stdout: $helperStdout"
Write-Host "Helper stderr: $helperStderr"
Write-Host "Helper touchpad log: $helperTouchpadLog"
Write-Host ""
Write-Host "Daemon stdout log: $daemonStdout"
Write-Host "Daemon runtime log: $daemonRuntimeLog"
Write-Host ""
Write-Host "Starting flowtile-core-daemon in the current console and saving logs to files..."
Write-Host "Stop with Ctrl+C. The helper will be stopped automatically."

Push-Location $repoRoot
$previousEarlyLogPath = $env:FLOWTILE_EARLY_LOG_PATH
try {
    $env:FLOWTILE_EARLY_LOG_PATH = $daemonRuntimeLog
    & $daemonExe @daemonArguments 2>&1 | Tee-Object -FilePath $daemonStdout
    if ($null -ne $LASTEXITCODE) {
        $daemonExitCode = $LASTEXITCODE
    } else {
        $daemonExitCode = 0
    }
    if ($daemonExitCode -ne 0) {
        throw "flowtile-core-daemon exited with code $daemonExitCode."
    }
}
finally {
    if ($null -ne $previousEarlyLogPath) {
        $env:FLOWTILE_EARLY_LOG_PATH = $previousEarlyLogPath
    } else {
        Remove-Item Env:FLOWTILE_EARLY_LOG_PATH -ErrorAction SilentlyContinue
    }
    Pop-Location
    if ($null -ne $helper -and -not $helper.HasExited) {
        Stop-Process -Id $helper.Id -Force -ErrorAction SilentlyContinue
    }
}
