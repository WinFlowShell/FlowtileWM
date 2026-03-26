param(
    [switch]$RunDaemon,
    [switch]$ForceNewCertificate,
    [switch]$DryRunCheck
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$targetDir = Join-Path $repoRoot "tmp\target-uiaccess-touchpad"
$stageDir = Join-Path $repoRoot "tmp\uiaccess-touchpad"
$certExportPath = Join-Path $stageDir "flowtile-uiaccess-dev.cer"
$earlyLogPath = Join-Path $stageDir "uiaccess-daemon-early.log"
$builtExe = Join-Path $targetDir "debug\flowtile-core-daemon.exe"
$installDir = Join-Path ${env:ProgramFiles} "FlowtileWM\uiaccess-dev"
$installedExe = Join-Path $installDir "flowtile-core-daemon.exe"
$certSubject = "CN=FlowtileWM UIAccess Dev"

function Test-IsAdministrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-OrCreateUiAccessCertificate {
    param(
        [string]$Subject,
        [string]$ExportPath,
        [switch]$ForceNew
    )

    $existing = Get-ChildItem Cert:\CurrentUser\My |
        Where-Object { $_.Subject -eq $Subject } |
        Sort-Object NotAfter -Descending |
        Select-Object -First 1

    if ($ForceNew -or $null -eq $existing) {
        Write-Host "Creating a new code-signing certificate for UIAccess dev-run..."
        $existing = New-SelfSignedCertificate `
            -Type CodeSigningCert `
            -Subject $Subject `
            -CertStoreLocation "Cert:\CurrentUser\My" `
            -KeyExportPolicy Exportable `
            -FriendlyName "FlowtileWM UIAccess Dev"
    }

    if (-not (Test-Path $stageDir)) {
        New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
    }

    Export-Certificate -Cert $existing -FilePath $ExportPath -Force | Out-Null
    return $existing
}

function Ensure-CertificateTrusted {
    param(
        [string]$CertificatePath,
        [System.Security.Cryptography.X509Certificates.X509Certificate2]$Certificate
    )

    $stores = @(
        "Cert:\LocalMachine\Root",
        "Cert:\LocalMachine\TrustedPublisher"
    )

    foreach ($store in $stores) {
        $present = Get-ChildItem $store |
            Where-Object { $_.Thumbprint -eq $Certificate.Thumbprint } |
            Select-Object -First 1
        if ($null -eq $present) {
            Write-Host "Importing certificate into $store..."
            Import-Certificate -FilePath $CertificatePath -CertStoreLocation $store | Out-Null
        }
    }
}

if (-not $IsWindows) {
    throw "This script only supports Windows."
}

if (-not (Test-IsAdministrator)) {
    throw "UIAccess deployment requires an elevated PowerShell session."
}

if (-not (Test-Path $stageDir)) {
    New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
}

$certificate = Get-OrCreateUiAccessCertificate `
    -Subject $certSubject `
    -ExportPath $certExportPath `
    -ForceNew:$ForceNewCertificate

Ensure-CertificateTrusted -CertificatePath $certExportPath -Certificate $certificate

Write-Host "Building flowtile-core-daemon with uiAccess manifest..."
$env:FLOWTILE_UIACCESS_MANIFEST = "1"
try {
    cargo build --target-dir $targetDir -p flowtile-core-daemon
}
finally {
    Remove-Item Env:FLOWTILE_UIACCESS_MANIFEST -ErrorAction SilentlyContinue
}

if (-not (Test-Path $builtExe)) {
    throw "Expected daemon executable was not built: $builtExe"
}

Write-Host "Signing built executable..."
$signature = Set-AuthenticodeSignature -FilePath $builtExe -Certificate $certificate -HashAlgorithm SHA256
if ($signature.Status -ne "Valid") {
    throw "Authenticode signing failed with status '$($signature.Status)'."
}

Write-Host "Deploying signed executable to $installDir..."
New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item -Path $builtExe -Destination $installedExe -Force

$installedSignature = Get-AuthenticodeSignature -FilePath $installedExe
if ($installedSignature.Status -ne "Valid") {
    throw "Installed executable signature is not valid: $($installedSignature.Status)"
}

Write-Host ""
Write-Host "UIAccess touchpad daemon is prepared."
Write-Host "Installed executable: $installedExe"
Write-Host "Working directory for runtime: $repoRoot"
Write-Host ""
Write-Host "Manual validation run:"
Write-Host "  & `"$installedExe`" watch --dry-run --poll-only"
Write-Host ""
Write-Host "Manual live run:"
Write-Host "  & `"$installedExe`" watch"

if ($DryRunCheck) {
    Write-Host ""
    Write-Host "Starting UIAccess daemon dry-run check..."
    Push-Location $repoRoot
    try {
        & $installedExe watch --dry-run --poll-only
    }
    finally {
        Pop-Location
    }
}

if ($RunDaemon) {
    Write-Host ""
    Write-Host "Starting UIAccess daemon live runtime..."
    Push-Location $repoRoot
    try {
        Remove-Item $earlyLogPath -ErrorAction SilentlyContinue
        $env:FLOWTILE_EARLY_LOG_PATH = $earlyLogPath
        & $installedExe watch
        $exitCode = $LASTEXITCODE
        if ($null -eq $exitCode) {
            $exitCode = 0
        }
        if ($exitCode -ne 0) {
            if (Test-Path $earlyLogPath) {
                Write-Host ""
                Write-Host "early runtime log: $earlyLogPath"
                Get-Content $earlyLogPath | ForEach-Object { Write-Host $_ }
            }
            throw "UIAccess daemon live runtime exited unexpectedly with code $exitCode."
        }

        Start-Sleep -Seconds 2
        $runningProcess = Get-CimInstance Win32_Process `
            -Filter "Name = 'flowtile-core-daemon.exe'" |
            Where-Object { $_.ExecutablePath -eq $installedExe } |
            Select-Object -First 1

        if ($null -eq $runningProcess) {
            if (Test-Path $earlyLogPath) {
                Write-Host ""
                Write-Host "early runtime log: $earlyLogPath"
                Get-Content $earlyLogPath | ForEach-Object { Write-Host $_ }
            }
            throw "UIAccess daemon launch returned success, but no live process was found for $installedExe."
        }

        Write-Host "UIAccess daemon is running. PID: $($runningProcess.ProcessId)"
    }
    finally {
        Remove-Item Env:FLOWTILE_EARLY_LOG_PATH -ErrorAction SilentlyContinue
        Pop-Location
    }
}
