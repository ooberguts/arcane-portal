param(
    [string]$Target = 'x86_64-pc-windows-msvc',
    [string]$Package = 'Arcane-Portal-Windows-x64'
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$binary = Join-Path $root "target/$Target/release/arcane-portal.exe"
$destination = Join-Path $root 'dist'
$stage = Join-Path $destination $Package
$archive = Join-Path $destination "$Package.zip"

if (!(Test-Path -LiteralPath $binary -PathType Leaf)) {
    throw "Build the release executable for $Target before packaging it."
}

New-Item -ItemType Directory -Path $stage -Force | Out-Null
$app = Join-Path $stage 'Arcane Portal.exe'
Copy-Item -LiteralPath $binary -Destination $app -Force
Compress-Archive -LiteralPath $app -DestinationPath $archive -Force
Write-Output $archive
