param(
    [switch]$NoBuild,
    [switch]$NoStart,
    [switch]$NoStopExisting,
    [switch]$DryRunWatch,
    [switch]$NoDiagnosticsCollectors,
    [switch]$EnableSurrogateDiagnostics,
    [ValidateRange(500, 60000)]
    [int]$DiagnosticsSampleIntervalMs = 2000
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$daemonTargetDir = Join-Path $repoRoot "tmp\target-touchpad-dev"
$helperBuildDir = Join-Path $repoRoot "tmp\touchpad-helper-build"
$runDir = Join-Path $repoRoot "tmp\touchpad-dev-run"
$daemonExe = Join-Path $daemonTargetDir "debug\flowtile-core-daemon.exe"
$cliExe = Join-Path $daemonTargetDir "debug\flowtile-cli.exe"
$helperDll = Join-Path $helperBuildDir "flowtile-touchpad-helper.dll"
$daemonStdout = Join-Path $runDir "daemon.stdout.log"
$daemonStderr = Join-Path $runDir "daemon.stderr.log"
$daemonRuntimeLog = Join-Path $runDir "daemon.runtime.log"
$helperStdout = Join-Path $runDir "helper.stdout.log"
$helperStderr = Join-Path $runDir "helper.stderr.log"
$helperTouchpadLog = Join-Path $runDir "touchpad-helper.log"
$touchpadDumpLog = Join-Path $runDir "touchpad.dump.log"
$collectorRuntimeLog = Join-Path $runDir "collector.runtime.log"
$ipcEventsLog = Join-Path $runDir "ipc.events.jsonl"
$processSamplesLog = Join-Path $runDir "process.samples.jsonl"
$diagnosticsSamplesLog = Join-Path $runDir "diagnostics.samples.jsonl"
$perfSamplesLog = Join-Path $runDir "perf.samples.jsonl"
$stateSnapshotsLog = Join-Path $runDir "state.snapshots.jsonl"
$sessionMetadataPath = Join-Path $runDir "dev-session.json"

function Ensure-Directory {
    param([string]$Path)

    if (-not (Test-Path $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Append-TimestampedLine {
    param(
        [string]$Path,
        [string]$Message
    )

    $timestamp = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    Add-Content -LiteralPath $Path -Value "[$timestamp] $Message"
}

function Write-JsonFile {
    param(
        [string]$Path,
        [hashtable]$Value
    )

    $Value | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $Path -Encoding utf8
}

function Stop-ExistingTouchpadDevProcesses {
    $knownExecutables = @(
        (Join-Path $daemonTargetDir "debug\flowtile-core-daemon.exe"),
        (Join-Path $daemonTargetDir "debug\flowtile-cli.exe"),
        (Join-Path $repoRoot "target\debug\flowtile-core-daemon.exe"),
        (Join-Path $repoRoot "apps\touchpad-helper\bin\Debug\net8.0-windows\flowtile-touchpad-helper.exe")
    )

    $candidateProcesses = Get-CimInstance Win32_Process |
        Where-Object {
            $_.Name -in @("flowtile-core-daemon.exe", "flowtile-cli.exe", "flowtile-touchpad-helper.exe", "dotnet.exe", "pwsh.exe", "powershell.exe")
        }

    foreach ($process in $candidateProcesses) {
        $commandLine = $process.CommandLine
        $executablePath = $process.ExecutablePath

        $isHelperDotnet = $process.Name -eq "dotnet.exe" -and
            $null -ne $commandLine -and
            $commandLine.Contains("Flowtile.TouchpadHelper.csproj")

        $isDevCollector = ($process.Name -in @("pwsh.exe", "powershell.exe")) -and
            $null -ne $commandLine -and
            ($commandLine.Contains($cliExe) -or $commandLine.Contains($runDir))

        $isKnownExecutable = $null -ne $executablePath -and $knownExecutables -contains $executablePath

        if (-not $isHelperDotnet -and -not $isKnownExecutable -and -not $isDevCollector) {
            continue
        }

        Write-Host "Stopping existing process $($process.Name) (PID $($process.ProcessId))..."
        Stop-Process -Id $process.ProcessId -Force -ErrorAction Stop
    }
}

function Start-DiagnosticsCollectors {
    param(
        [string]$CliPath,
        [string]$RepoRoot,
        [int]$DaemonPid,
        [int]$HelperPid,
        [string]$CollectorLogPath,
        [string]$EventsLogPath,
        [string]$ProcessLogPath,
        [string]$DiagnosticsLogPath,
        [string]$PerfLogPath,
        [string]$StateLogPath,
        [int]$SampleIntervalMs
    )

    $collectorScript = Join-Path $PSScriptRoot "run-dev-runtime-collector.ps1"
    if (-not (Test-Path $collectorScript)) {
        throw "Collector helper script was not found: $collectorScript"
    }

    $collectors = @()
    $commonArguments = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $collectorScript,
        "-CliPath", $CliPath,
        "-RepoRoot", $RepoRoot,
        "-DaemonPid", $DaemonPid,
        "-CollectorLogPath", $CollectorLogPath
    )

    $collectors += Start-Process `
        -FilePath "pwsh" `
        -ArgumentList @(
            $commonArguments +
            @(
                "-Mode", "event-stream",
                "-EventsLogPath", $EventsLogPath
            )
        ) `
        -WorkingDirectory $RepoRoot `
        -WindowStyle Hidden `
        -PassThru

    $collectors += Start-Process `
        -FilePath "pwsh" `
        -ArgumentList @(
            $commonArguments +
            @(
                "-Mode", "sampling",
                "-HelperPid", $HelperPid,
                "-ProcessLogPath", $ProcessLogPath,
                "-DiagnosticsLogPath", $DiagnosticsLogPath,
                "-PerfLogPath", $PerfLogPath,
                "-StateLogPath", $StateLogPath,
                "-SampleIntervalMs", $SampleIntervalMs
            )
        ) `
        -WorkingDirectory $RepoRoot `
        -WindowStyle Hidden `
        -PassThru

    return $collectors
}

function Stop-DiagnosticsCollectors {
    param([System.Diagnostics.Process[]]$Jobs)

    foreach ($job in $Jobs) {
        if ($null -eq $job) {
            continue
        }

        Stop-Process -Id $job.Id -Force -ErrorAction SilentlyContinue
    }
}

Ensure-Directory $runDir

if (-not $NoStopExisting) {
    Stop-ExistingTouchpadDevProcesses
}

if (-not $NoBuild) {
    Write-Host "Building flowtile-core-daemon and flowtile-cli into $daemonTargetDir..."
    cargo build --target-dir $daemonTargetDir -p flowtile-core-daemon -p flowtile-cli

    Write-Host "Building touchpad-helper into $helperBuildDir..."
    dotnet build (Join-Path $repoRoot "apps\touchpad-helper\Flowtile.TouchpadHelper.csproj") -o $helperBuildDir
}

if (-not (Test-Path $daemonExe)) {
    throw "Daemon executable was not found: $daemonExe"
}

if (-not (Test-Path $helperDll)) {
    throw "Touchpad helper build output was not found: $helperDll"
}

if (-not $NoStart -and -not $NoDiagnosticsCollectors -and -not (Test-Path $cliExe)) {
    throw "CLI executable was not found: $cliExe"
}

Write-Host ""
Write-Host "Dev runtime entrypoint is prepared."
Write-Host "Daemon executable: $daemonExe"
Write-Host "CLI executable: $cliExe"
Write-Host "Helper DLL: $helperDll"
Write-Host "Run logs: $runDir"
Write-Host ""
Write-Host "Manual daemon run:"
Write-Host "  & `"$daemonExe`" watch"
Write-Host ""
Write-Host "Manual helper run:"
Write-Host "  dotnet `"$helperDll`""
Write-Host ""
Write-Host "Diagnostics bundle:"
Write-Host "  daemon stdout: $daemonStdout"
Write-Host "  daemon stderr: $daemonStderr"
Write-Host "  daemon runtime: $daemonRuntimeLog"
Write-Host "  touchpad dump: $touchpadDumpLog"
Write-Host "  ipc events: $ipcEventsLog"
Write-Host "  process samples: $processSamplesLog"
Write-Host "  diagnostics samples: $diagnosticsSamplesLog"
Write-Host "  perf samples: $perfSamplesLog"
Write-Host "  state snapshots: $stateSnapshotsLog"
Write-Host "  collector runtime: $collectorRuntimeLog"
Write-Host "  surrogate diagnostics: $EnableSurrogateDiagnostics"

if ($NoStart) {
    return
}

Remove-Item `
    $daemonStdout,
    $daemonStderr,
    $daemonRuntimeLog,
    $helperStdout,
    $helperStderr,
    $helperTouchpadLog,
    $touchpadDumpLog,
    $collectorRuntimeLog,
    $ipcEventsLog,
    $processSamplesLog,
    $diagnosticsSamplesLog,
    $perfSamplesLog,
    $stateSnapshotsLog,
    $sessionMetadataPath `
    -ErrorAction SilentlyContinue

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
Write-Host "Touchpad dump log: $touchpadDumpLog"

Write-Host ""
Write-Host "Starting flowtile-core-daemon and streaming stdout into the current console..."
Write-Host "Stop with Ctrl+C. The helper and collectors will be stopped automatically."

$daemon = Start-Process `
    -FilePath $daemonExe `
    -ArgumentList $daemonArguments `
    -WorkingDirectory $repoRoot `
    -NoNewWindow `
    -RedirectStandardOutput $daemonStdout `
    -RedirectStandardError $daemonStderr `
    -Environment @{
        FLOWTILE_EARLY_LOG_PATH = $daemonRuntimeLog
        FLOWTILE_TOUCHPAD_DUMP_PATH = $touchpadDumpLog
        FLOWTILE_CLIPPED_WINDOW_SURROGATE_DIAGNOSTICS = if ($EnableSurrogateDiagnostics) { "1" } else { "0" }
        RUST_BACKTRACE = "1"
    } `
    -PassThru

$collectorJobs = @()
if (-not $NoDiagnosticsCollectors) {
    $collectorJobs = @(Start-DiagnosticsCollectors `
        -CliPath $cliExe `
        -RepoRoot $repoRoot `
        -DaemonPid $daemon.Id `
        -HelperPid $helper.Id `
        -CollectorLogPath $collectorRuntimeLog `
        -EventsLogPath $ipcEventsLog `
        -ProcessLogPath $processSamplesLog `
        -DiagnosticsLogPath $diagnosticsSamplesLog `
        -PerfLogPath $perfSamplesLog `
        -StateLogPath $stateSnapshotsLog `
        -SampleIntervalMs $DiagnosticsSampleIntervalMs)
}

Write-JsonFile -Path $sessionMetadataPath -Value @{
    started_at = (Get-Date).ToString("o")
    repo_root = $repoRoot
    dry_run_watch = [bool]$DryRunWatch
    diagnostics_collectors_enabled = -not $NoDiagnosticsCollectors
    surrogate_diagnostics_enabled = [bool]$EnableSurrogateDiagnostics
    diagnostics_sample_interval_ms = $DiagnosticsSampleIntervalMs
    executables = @{
        daemon = $daemonExe
        cli = $cliExe
        helper_dll = $helperDll
    }
    run_dir = $runDir
    logs = @{
        daemon_stdout = $daemonStdout
        daemon_stderr = $daemonStderr
        daemon_runtime = $daemonRuntimeLog
        helper_stdout = $helperStdout
        helper_stderr = $helperStderr
        helper_touchpad = $helperTouchpadLog
        touchpad_dump = $touchpadDumpLog
        collector_runtime = $collectorRuntimeLog
        ipc_events = $ipcEventsLog
        process_samples = $processSamplesLog
        diagnostics_samples = $diagnosticsSamplesLog
        perf_samples = $perfSamplesLog
        state_snapshots = $stateSnapshotsLog
    }
    pids = @{
        daemon = $daemon.Id
        helper = $helper.Id
        collector_processes = @($collectorJobs | ForEach-Object { $_.Id })
    }
}

Write-Host ""
Write-Host "Daemon PID: $($daemon.Id)"
Write-Host "Daemon stdout log: $daemonStdout"
Write-Host "Daemon stderr log: $daemonStderr"
Write-Host "Daemon runtime log: $daemonRuntimeLog"
Write-Host "Session metadata: $sessionMetadataPath"

if (-not $NoDiagnosticsCollectors) {
    Write-Host ""
    Write-Host "Diagnostics collectors are running."
    Write-Host "Collector runtime log: $collectorRuntimeLog"
    Write-Host "IPC events log: $ipcEventsLog"
    Write-Host "Process samples log: $processSamplesLog"
    Write-Host "Diagnostics samples log: $diagnosticsSamplesLog"
    Write-Host "Perf samples log: $perfSamplesLog"
    Write-Host "State snapshots log: $stateSnapshotsLog"
}

$printedStdoutLines = 0
try {
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
    Stop-DiagnosticsCollectors -Jobs $collectorJobs
    if ($null -ne $daemon -and -not $daemon.HasExited) {
        Stop-Process -Id $daemon.Id -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $helper -and -not $helper.HasExited) {
        Stop-Process -Id $helper.Id -Force -ErrorAction SilentlyContinue
    }
}
