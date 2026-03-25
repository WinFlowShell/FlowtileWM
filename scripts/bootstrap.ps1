Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Assert-Command {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    if (-not (Get-Command -Name $Name -ErrorAction SilentlyContinue)) {
        throw "Required command '$Name' was not found in PATH."
    }
}

Assert-Command -Name pwsh
Assert-Command -Name cargo
Assert-Command -Name cargo-clippy
Assert-Command -Name cargo-fmt
$pwshVersion = [Version]$PSVersionTable.PSVersion.ToString()
if ($pwshVersion.Major -lt 7) {
    throw "PowerShell 7+ is required. Current version: $pwshVersion"
}

Write-Host "Tool versions:"
Write-Host ("  cargo: " + (cargo --version))
Write-Host ("  cargo-clippy: " + (cargo clippy --version))
Write-Host ("  cargo-fmt: " + (cargo fmt --version))

Write-Host "Rust workspace metadata:"
cargo metadata --format-version 1 --no-deps | Out-Null
Write-Host "  cargo metadata: ok"

Write-Host "Bootstrap checks completed."
