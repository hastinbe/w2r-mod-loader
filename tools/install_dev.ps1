<#
.SYNOPSIS
Developer install of the W2R Mod Loader host into the game, with the basic fixture plugin.

.DESCRIPTION
Builds w2r_core.dll (release, i686) and copies it into the game's x86 folder as version.dll.
Sets up the host root under Saved Games\Warcraft2Remastered\w2r with the basic_plugin fixture
(plugin.json, DLL, pinned hash, chat.write approved). Then launch the game, enter a multiplayer
lobby, and read logs\w2r.log: the build guard should report PASSED (205/205) and the plugin
should log "basic from basic_plugin".

.PARAMETER Uninstall
Removes our version.dll from the game folder and restores version.dll.bak if an earlier
install had backed one up (the Saved Games root is left alone). A version.dll that is not ours
is left in place.

.PARAMETER WithCrasher
Also installs the crash_plugin fixture, which faults on lobby entry; the log should show it
disabled while the game and basic_plugin carry on.

.PARAMETER WithHostCheck
Also installs the hostcheck_plugin fixture (game.read and hooks.raw approved; it installs no
hook). Its load logs a settings round trip, a data file written and read back, and the runtime
address of three symbols; it logs again when a match starts.

.PARAMETER WithChat
Also builds and installs the first-party chat plugin (plugins/w2r-chat in the w2r-plugins
repository, see -PluginsRepo; id w2r-chat; game.read
and hooks.raw approved). It hooks the game's in-match message functions, so it is a "dangerous"
plugin and is in the simulation fingerprint.

.PARAMETER WithAllPlugins
Builds and installs every plugin under plugins\ in the w2r-plugins repository (see -PluginsRepo),
each with its own plugin.json and the capabilities that manifest asks for approved. Covers
-WithChat, which need not be passed with it.

.PARAMETER Simulation
Marks the basic_plugin fixture affects_simulation, so the session's simulation fingerprint is
non-zero and the handshake (Phase 3) has something to compare: the log shows the fingerprint,
the plugin screen shows it, and a second player whose fingerprint differs gets the lobby
warning.

.PARAMETER PluginsRepo
The w2r-plugins checkout the chat plugin is built from. Defaults to a sibling folder of this
repository (..\w2r-plugins).

.PARAMETER GameDir
The game's x86 folder. Defaults to D:\Games\Warcraft II Remastered\x86.
#>
param(
    [switch]$Uninstall,
    [switch]$WithCrasher,
    [switch]$WithHostCheck,
    [switch]$WithChat,
    [switch]$WithAllPlugins,
    [switch]$Simulation,
    [string]$PluginsRepo = "",
    [string]$GameDir = "D:\Games\Warcraft II Remastered\x86"
)
$ErrorActionPreference = "Stop"

$repo = Split-Path -Parent $PSScriptRoot
if (-not $PluginsRepo) { $PluginsRepo = Join-Path (Split-Path -Parent $repo) "w2r-plugins" }
$target = Join-Path $GameDir "version.dll"
$backup = Join-Path $GameDir "version.dll.bak"
$oldBackup = Join-Path $GameDir "version.dll.pre-w2rpm"  # name used by earlier versions

# Our DLL carries this literal (the first log line); other proxies (wc2r-plus uses the same
# file name) do not, so the script never deletes someone else's mod.
function Test-OurDll([string]$Path) {
    if (-not (Test-Path $Path)) { return $false }
    # ISO-8859-1 maps every byte to one char; Windows PowerShell 5.1 has no Latin1 property.
    $text = [Text.Encoding]::GetEncoding(28591).GetString([IO.File]::ReadAllBytes($Path))
    return $text.Contains("=== w2rpm host ")
}

if ($Uninstall) {
    if (-not (Test-Path $target)) {
        Write-Host "nothing installed at $target"
    } elseif (-not (Test-OurDll $target)) {
        Write-Host "$target is not the W2R Mod Loader host; left in place"
    } else {
        Remove-Item $target
        Write-Host "removed $target"
        if (-not (Test-Path $backup) -and (Test-Path $oldBackup)) { $backup = $oldBackup }
        if (Test-Path $backup) { Move-Item $backup $target; Write-Host "restored the previous version.dll from $backup" }
    }
    exit 0
}

if (-not (Test-Path (Join-Path $GameDir "Warcraft II.exe"))) {
    throw "Warcraft II.exe not found in $GameDir"
}

Push-Location $repo
try {
    cargo build --release -p w2r-core --target i686-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "host build failed" }
    cargo build --release -p w2r-test-plugins --examples --target i686-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw "fixture build failed" }
} finally {
    Pop-Location
}

if ($WithChat) {
    if (-not (Test-Path (Join-Path $PluginsRepo "plugins\w2r-chat"))) {
        throw "w2r-chat not found in $PluginsRepo; clone w2r-plugins next to this repository or pass -PluginsRepo"
    }
    Push-Location $PluginsRepo
    try {
        cargo build --release -p w2r-chat --target i686-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw "chat plugin build failed" }
    } finally {
        Pop-Location
    }
}

$allPlugins = @()
if ($WithAllPlugins) {
    $allPlugins = @(Get-ChildItem (Join-Path $PluginsRepo "plugins") -Directory | Where-Object { $_.Name -ne "w2r-chat" } | ForEach-Object { $_.Name })
    if (-not $WithChat) { $allPlugins += "w2r-chat" }
    Push-Location $PluginsRepo
    try {
        foreach ($id in $allPlugins) {
            cargo build --release -p $id --target i686-pc-windows-msvc
            if ($LASTEXITCODE -ne 0) { throw "$id build failed" }
        }
    } finally {
        Pop-Location
    }
}

$dll = Join-Path $repo "target\i686-pc-windows-msvc\release\w2r_core.dll"
$examples = Join-Path $repo "target\i686-pc-windows-msvc\release\examples"
if ((Test-Path $target) -and -not (Test-OurDll $target)) {
    if (Test-Path $backup) { throw "$target is not ours and $backup already exists; sort that out by hand" }
    Move-Item $target $backup
    Write-Host "kept the existing version.dll (another mod?) as $backup"
}
Copy-Item $dll $target -Force
Write-Host "installed $target"

# Saved Games may be relocated; ask the shell, fall back to the default.
$savedGames = $null
try {
    $shellFolders = Get-ItemProperty "HKCU:\Software\Microsoft\Windows\CurrentVersion\Explorer\User Shell Folders"
    $savedGames = [Environment]::ExpandEnvironmentVariables($shellFolders.'{4C5C32FF-BB9D-43B0-B5B4-2D72E54EAAA4}')
} catch {}
if (-not $savedGames) { $savedGames = Join-Path $env:USERPROFILE "Saved Games" }
$root = Join-Path $savedGames "Warcraft2Remastered\w2r"
foreach ($sub in "plugins", "config", "cache", "logs") {
    New-Item -ItemType Directory -Force (Join-Path $root $sub) | Out-Null
}

function Install-Fixture([string]$Id, [string]$Example, [string[]]$Capabilities, [bool]$AffectsSimulation = $false, [string]$From = $examples, [string]$Author = "Beau Hastings", [string]$Description = "Development fixture installed by tools/install_dev.ps1.") {
    $folder = Join-Path $root "plugins\$Id"
    New-Item -ItemType Directory -Force $folder | Out-Null
    $entry = "$Example.dll"
    Copy-Item (Join-Path $From $entry) (Join-Path $folder $entry) -Force
    $capsJson = ($Capabilities | ForEach-Object { '"' + $_ + '"' }) -join ", "
    $simJson = if ($AffectsSimulation) { "true" } else { "false" }
    $manifest = @"
{
  "schema": 1,
  "id": "$Id",
  "name": "$Example (fixture)",
  "version": "1.0.0",
  "author": "$Author",
  "description": "$Description",
  "entry": "$entry",
  "abi": 1,
  "game_builds": ["1.0.2.2818"],
  "capabilities": [$capsJson],
  "affects_simulation": $simJson,
  "dependencies": []
}
"@
    [IO.File]::WriteAllText((Join-Path $folder "plugin.json"), $manifest, [Text.UTF8Encoding]::new($false))
    $hash = (Get-FileHash -Algorithm SHA256 (Join-Path $folder $entry)).Hash.ToLower()
    return @"
    "$Id": {
      "version": "1.0.0",
      "enabled": true,
      "sha256": "$hash",
      "approved_capabilities": [$capsJson]
    }
"@
}

$entries = @(Install-Fixture "basic-plugin" "basic_plugin" @("chat.write") $Simulation.IsPresent)
if ($WithCrasher) { $entries += Install-Fixture "crash-plugin" "crash_plugin" @() }
if ($WithChat) {
    $release = Join-Path $PluginsRepo "target\i686-pc-windows-msvc\release"
    $entries += Install-Fixture "w2r-chat" "w2r_chat" @("game.read", "hooks.raw") $false $release "Luxray997 (ported by Beau Hastings)" "In-match chat history with channels and player colours (port of wc2r-plus chat.cpp by Luxray997)."
}
# A plugin switched off in the GUI stays off.
$disabledBefore = @()
$oldState = Join-Path $root "config\state.json"
if (Test-Path $oldState) {
    $prev = Get-Content $oldState -Raw | ConvertFrom-Json
    $disabledBefore = @($prev.plugins.PSObject.Properties | Where-Object { $_.Value.enabled -eq $false } | ForEach-Object { $_.Name })
}
foreach ($id in $allPlugins) {
    # The plugin's own manifest goes in unchanged; only the pin and approvals are written here.
    $source = Join-Path $PluginsRepo "plugins\$id"
    $manifest = Get-Content (Join-Path $source "plugin.json") -Raw | ConvertFrom-Json
    $folder = Join-Path $root "plugins\$id"
    New-Item -ItemType Directory -Force $folder | Out-Null
    Copy-Item (Join-Path $source "plugin.json") (Join-Path $folder "plugin.json") -Force
    Copy-Item (Join-Path $PluginsRepo ("target\i686-pc-windows-msvc\release\" + $manifest.entry)) (Join-Path $folder $manifest.entry) -Force
    $hash = (Get-FileHash -Algorithm SHA256 (Join-Path $folder $manifest.entry)).Hash.ToLower()
    $capsJson = ($manifest.capabilities | ForEach-Object { '"' + $_ + '"' }) -join ", "
    $enabledText = if ($disabledBefore -contains $id) { "false" } else { "true" }
    $entries += @"
    "$id": {
      "version": "$($manifest.version)",
      "enabled": $enabledText,
      "sha256": "$hash",
      "approved_capabilities": [$capsJson]
    }
"@
}
if ($WithHostCheck) { $entries += Install-Fixture "host-check" "hostcheck_plugin" @("game.read", "hooks.raw") }
$state = "{`n  `"schema`": 1,`n  `"plugins`": {`n" + ($entries -join ",`n") + "`n  }`n}`n"
$statePath = Join-Path $root "config\state.json"
if (Test-Path $statePath) {
    # The fixture record replaces whatever was there: keep the user's pins and approvals.
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    Copy-Item $statePath "$statePath.$stamp.bak"
    Write-Host "backed up the existing state.json to state.json.$stamp.bak"
}
[IO.File]::WriteAllText($statePath, $state, [Text.UTF8Encoding]::new($false))

Write-Host "host root: $root"
Write-Host "now launch the game, enter a multiplayer lobby, then read $root\logs\w2r.log"
Write-Host "the handshake logs 'announced (nonce ...)' on lobby entry; config\host.json holds the host settings"
