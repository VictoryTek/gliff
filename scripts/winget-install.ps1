<#
.SYNOPSIS
  Install or upgrade the gliff Windows viewer with winget, from a GitHub release.

.DESCRIPTION
  Downloads vexdesk-<version>-x86_64-windows.zip from the repo's win-v<version> release,
  writes a winget manifest for it (a portable app, command: vexdesk) and
  runs `winget install --manifest`. Run it again with a newer -Version to upgrade.

  winget only installs from a local manifest after this is run once in an
  elevated terminal:  winget settings --enable LocalManifestFiles

.EXAMPLE
  .\scripts\winget-install.ps1              # the latest release
  .\scripts\winget-install.ps1 -Version 0.1.0
#>
param(
    [string]$Version,
    [string]$Repo = 'VictoryTek/vexdesk',
    # Hash this local zip instead of downloading the release's (for testing;
    # the manifest still names the release URL).
    [string]$ZipFile,
    # Write and validate the manifest, but do not install.
    [switch]$ManifestOnly
)
$ErrorActionPreference = 'Stop'

$settings = winget settings export | ConvertFrom-Json
if (-not $ManifestOnly -and -not $settings.adminSettings.LocalManifestFiles) {
    Write-Error "winget has local manifests turned off. In an ELEVATED terminal run: winget settings --enable LocalManifestFiles"
}

if (-not $Version -and $ZipFile) {
    Write-Error "-ZipFile needs -Version"
}
if (-not $Version) {
    # The fork's viewer releases are tagged win-v<version>; its other releases
    # (upstream-style v* tags) are not ours to install.
    $releases = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases?per_page=30"
    $latest = $releases | Where-Object { $_.tag_name -like 'win-v*' -and -not $_.draft -and -not $_.prerelease } | Select-Object -First 1
    if (-not $latest) {
        Write-Error "no win-v* release in $Repo yet"
    }
    $Version = $latest.tag_name -replace '^win-v', ''
}
$zipName = "vexdesk-$Version-x86_64-windows.zip"
$url = "https://github.com/$Repo/releases/download/win-v$Version/$zipName"

$work = Join-Path ([IO.Path]::GetTempPath()) "vexdesk-winget-$Version"
Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $work | Out-Null

if ($ZipFile) {
    $zip = $ZipFile
} else {
    $zip = Join-Path $work $zipName
    Write-Host "Downloading $url"
    Invoke-WebRequest $url -OutFile $zip
}
$sha = (Get-FileHash $zip -Algorithm SHA256).Hash

$id = 'VictoryTek.VexDesk'
$schema = '1.6.0'
Set-Content (Join-Path $work "$id.yaml") -Encoding utf8 @"
PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
"@
Set-Content (Join-Path $work "$id.locale.en-US.yaml") -Encoding utf8 @"
PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: VictoryTek
PackageName: VexDesk
ShortDescription: Native Windows viewer for gliff-server remote desktops
License: MIT
Moniker: vexdesk
ManifestType: defaultLocale
ManifestVersion: $schema
"@
Set-Content (Join-Path $work "$id.installer.yaml") -Encoding utf8 @"
PackageIdentifier: $id
PackageVersion: $Version
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: vexdesk.exe
  PortableCommandAlias: vexdesk
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $sha
ManifestType: installer
ManifestVersion: $schema
"@

if ($ManifestOnly) {
    winget validate --manifest $work
    Write-Host "Manifest in $work"
    exit $LASTEXITCODE
}
winget install --manifest $work --accept-source-agreements
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Write-Host "Installed. Open a new terminal and run: vexdesk user@host"
