param(
    [switch]$NoBuild,
    [switch]$NoStopExisting,
    [switch]$PollOnly,
    [switch]$DryRun,
    [ValidateRange(1, 1000000)]
    [int]$Iterations = 0,
    [ValidateRange(50, 60000)]
    [int]$IntervalMs = 750
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$targetDir = Join-Path $repoRoot "tmp\target-wm-only"
$runDir = Join-Path $repoRoot "tmp\wm-only-run"
$daemonExe = Join-Path $targetDir "debug\flowtile-core-daemon.exe"
$daemonStdout = Join-Path $runDir "daemon.stdout.log"
$daemonStderr = Join-Path $runDir "daemon.stderr.log"
$daemonRuntimeLog = Join-Path $runDir "daemon.runtime.log"
$sessionMetadataPath = Join-Path $runDir "session.json"

function Ensure-Directory {
    param([string]$Path)

    if (-not (Test-Path $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Write-JsonFile {
    param(
        [string]$Path,
        [hashtable]$Value
    )

    $Value | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Path -Encoding utf8
}

function Stop-ExistingWmOnlyProcesses {
    $candidateProcesses = Get-CimInstance Win32_Process |
        Where-Object {
            $_.Name -in @("flowtile-core-daemon.exe", "flowtile-touchpad-helper.exe", "dotnet.exe")
        }

    foreach ($process in $candidateProcesses) {
        $commandLine = $process.CommandLine

        $isDaemon = $process.Name -eq "flowtile-core-daemon.exe"
        $isHelperExe = $process.Name -eq "flowtile-touchpad-helper.exe"
        $isHelperDotnet = $process.Name -eq "dotnet.exe" -and
            $null -ne $commandLine -and
            (
                $commandLine.Contains("Flowtile.TouchpadHelper.csproj") -or
                $commandLine.Contains("flowtile-touchpad-helper.dll")
            )

        if (-not $isDaemon -and -not $isHelperExe -and -not $isHelperDotnet) {
            continue
        }

        Write-Host "Stopping existing process $($process.Name) (PID $($process.ProcessId))..."
        Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop
    }
}

Ensure-Directory $runDir

if (-not $NoStopExisting) {
    Stop-ExistingWmOnlyProcesses
}

if (-not $NoBuild) {
    Write-Host "Building flowtile-core-daemon into $targetDir..."
    cargo build --target-dir $targetDir -p flowtile-core-daemon
}

if (-not (Test-Path $daemonExe)) {
    throw "Daemon executable was not found: $daemonExe"
}

[string[]]$daemonArguments = @("watch", "wm-only")
if ($DryRun) {
    $daemonArguments += "--dry-run"
}
if ($PollOnly) {
    $daemonArguments += "--poll-only"
}
if ($Iterations -gt 0) {
    $daemonArguments += @("--iterations", $Iterations.ToString())
}
if ($IntervalMs -ne 750) {
    $daemonArguments += @("--interval-ms", $IntervalMs.ToString())
}

Write-Host ""
Write-Host "WM-only helper is prepared."
Write-Host "This script is a local direct runner for FlowtileWM only."
Write-Host "Canonical incident/live analysis launch remains:"
Write-Host "  P:\\Projects\\FlowShell\\FlowShellCore\\scripts\\start-flowshellcore.ps1 -Log"
Write-Host ""
Write-Host "Daemon executable: $daemonExe"
Write-Host "Run logs: $runDir"
Write-Host "Command line: $($daemonArguments -join ' ')"
Write-Host ""
Write-Host "This helper does not start touchpad-helper, diagnostics collectors, or companion shell surfaces."

Remove-Item `
    $daemonStdout,
    $daemonStderr,
    $daemonRuntimeLog,
    $sessionMetadataPath `
    -ErrorAction SilentlyContinue

$daemon = $null
$printedStdoutLines = 0

try {
    Write-Host ""
    Write-Host "Starting flowtile-core-daemon in wm-only mode..."
    Write-Host "Stop with Ctrl+C."

    $daemon = Start-Process `
        -FilePath $daemonExe `
        -ArgumentList $daemonArguments `
        -WorkingDirectory $repoRoot `
        -NoNewWindow `
        -RedirectStandardOutput $daemonStdout `
        -RedirectStandardError $daemonStderr `
        -Environment @{
            FLOWTILE_EARLY_LOG_PATH = $daemonRuntimeLog
            RUST_BACKTRACE = "1"
        } `
        -PassThru

    Write-JsonFile -Path $sessionMetadataPath -Value @{
        started_at = (Get-Date).ToString("o")
        repo_root = $repoRoot
        target_dir = $targetDir
        run_dir = $runDir
        daemon_executable = $daemonExe
        daemon_arguments = @($daemonArguments)
        logs = @{
            daemon_stdout = $daemonStdout
            daemon_stderr = $daemonStderr
            daemon_runtime = $daemonRuntimeLog
        }
        pid = $daemon.Id
    }

    Write-Host ""
    Write-Host "Daemon PID: $($daemon.Id)"
    Write-Host "Daemon stdout log: $daemonStdout"
    Write-Host "Daemon stderr log: $daemonStderr"
    Write-Host "Daemon runtime log: $daemonRuntimeLog"
    Write-Host "Session metadata: $sessionMetadataPath"

    while (-not $daemon.HasExited) {
        if (Test-Path $daemonStdout) {
            $stdoutLines = @(Get-Content -LiteralPath $daemonStdout)
            if ($stdoutLines.Count -gt $printedStdoutLines) {
                for ($i = $printedStdoutLines; $i -lt $stdoutLines.Count; $i++) {
                    Write-Host $stdoutLines[$i]
                }
                $printedStdoutLines = $stdoutLines.Count
            }
        }

        Start-Sleep -Milliseconds 250
        $daemon.Refresh()
    }

    if (Test-Path $daemonStdout) {
        $stdoutLines = @(Get-Content -LiteralPath $daemonStdout)
        if ($stdoutLines.Count -gt $printedStdoutLines) {
            for ($i = $printedStdoutLines; $i -lt $stdoutLines.Count; $i++) {
                Write-Host $stdoutLines[$i]
            }
            $printedStdoutLines = $stdoutLines.Count
        }
    }

    $daemonExitCode = $daemon.ExitCode
    if (Test-Path $daemonStderr) {
        $stderrLines = @(Get-Content -LiteralPath $daemonStderr)
        if ($stderrLines.Count -gt 0) {
            Write-Host ""
            Write-Host "Daemon stderr:"
            $stderrLines | ForEach-Object { Write-Host $_ }
        }
    }

    if ($daemonExitCode -ne 0) {
        throw "flowtile-core-daemon exited with code $daemonExitCode."
    }
}
finally {
    if ($null -ne $daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
    }
}
