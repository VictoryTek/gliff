<#
.SYNOPSIS
  Install or upgrade the gliff Windows viewer with winget, from a GitHub release.

.DESCRIPTION
  Downloads gliff-win-<version>-x86_64-windows.zip from the repo's release,
  writes a winget manifest for it (a portable app, command: gliff-win) and
  runs `winget install --manifest`. Run it again with a newer -Version to upgrade.

  winget only installs from a local manifest after this is run once in an
  elevated terminal:  winget settings --enable LocalManifestFiles

.EXAMPLE
  .\scripts\winget-install.ps1              # the latest release
  .\scripts\winget-install.ps1 -Version 0.4.4
#>
param(
    [string]$Version,
    [string]$Repo = 'VictoryTek/gliff',
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
    $latest = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $latest.tag_name.TrimStart('v')
}
$zipName = "gliff-win-$Version-x86_64-windows.zip"
$url = "https://github.com/$Repo/releases/download/v$Version/$zipName"

$work = Join-Path ([IO.Path]::GetTempPath()) "gliff-winget-$Version"
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

$id = 'VictoryTek.GliffWin'
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
PackageName: gliff-win
ShortDescription: Native Windows viewer for gliff remote desktops
License: MIT
Moniker: gliff-win
ManifestType: defaultLocale
ManifestVersion: $schema
"@
Set-Content (Join-Path $work "$id.installer.yaml") -Encoding utf8 @"
PackageIdentifier: $id
PackageVersion: $Version
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
- RelativeFilePath: gliff-win.exe
  PortableCommandAlias: gliff-win
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
Write-Host "Installed. Open a new terminal and run: gliff-win user@host"
