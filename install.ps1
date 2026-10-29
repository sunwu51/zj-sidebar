<#
.SYNOPSIS
  Install zj-sidebar (a zellij plugin) on Windows.

.DESCRIPTION
  Installs <zellij config dir>\plugins\zj-sidebar.wasm, the layout
  <zellij config dir>\layouts\sidebar.kdl and the native helper
  %LOCALAPPDATA%\zj-sidebar\zjsb-helper.exe. It never edits your config.kdl.

  By default it downloads a prebuilt release; -FromSource builds a checkout with cargo.

.EXAMPLE
  irm https://raw.githubusercontent.com/sunwu51/zj-sidebar/main/install.ps1 | iex
.EXAMPLE
  ./install.ps1 -FromSource -PatchProfile

.PARAMETER PatchProfile
  Append the pwsh hook that lets zellij see the directory of each pane (PowerShell's
  `cd` does not change the process working directory) to your $PROFILE.

  Environment (for `irm | iex`): ZJSB_REPO, ZJSB_VERSION, ZJSB_BASE_URL, ZELLIJ_CONFIG_DIR
#>
param(
    [switch]$FromSource,
    [string]$Dist,
    [string]$ConfigDir,
    [string]$Version,
    [string]$Repo,
    [string]$BaseUrl,
    [switch]$PatchProfile
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

if (-not $Repo) { $Repo = if ($env:ZJSB_REPO) { $env:ZJSB_REPO } else { 'sunwu51/zj-sidebar' } }
if (-not $Version) { $Version = if ($env:ZJSB_VERSION) { $env:ZJSB_VERSION } else { 'latest' } }
if (-not $BaseUrl) { $BaseUrl = $env:ZJSB_BASE_URL }
if (-not $ConfigDir) { $ConfigDir = $env:ZELLIJ_CONFIG_DIR }
if (-not $ConfigDir -and (Get-Command zellij -ErrorAction SilentlyContinue)) {
    # ask zellij itself where its config lives
    $line = (zellij setup --check 2>$null) | Where-Object { $_ -match '^\[CONFIG DIR\]: "(.*)"$' } | Select-Object -First 1
    if ($line -match '^\[CONFIG DIR\]: "(.*)"$') { $ConfigDir = $Matches[1] }
}
if (-not $ConfigDir) { $ConfigDir = Join-Path $env:APPDATA 'Zellij\config' }
$DataDir = Join-Path $env:LOCALAPPDATA 'zj-sidebar'
$Target = 'x86_64-pc-windows-msvc'

function Say($m) { Write-Host "==> $m" }

# plain .NET so it works in any PowerShell version
function Get-Sha256($path) {
    $sha = [Security.Cryptography.SHA256]::Create()
    $fs = [IO.File]::OpenRead($path)
    try { ([BitConverter]::ToString($sha.ComputeHash($fs)) -replace '-', '').ToLower() }
    finally { $fs.Dispose(); $sha.Dispose() }
}

$work = Join-Path ([IO.Path]::GetTempPath()) ("zjsb-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $work | Out-Null
try {
    if ($Dist) {
        Say "using local files from $Dist"
        Copy-Item (Join-Path $Dist 'zj-sidebar.wasm') (Join-Path $work 'zj-sidebar.wasm')
        Copy-Item (Join-Path $Dist "zjsb-helper-$Target.exe") (Join-Path $work 'zjsb-helper.exe')
    }
    elseif ($FromSource) {
        $src = $PSScriptRoot
        if (-not $src -or -not (Test-Path (Join-Path $src 'Cargo.toml'))) { throw '-FromSource must be run from a checkout of the repository' }
        if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { throw 'cargo not found (install Rust from https://rustup.rs)' }
        Say 'building the plugin (wasm32-wasip1)'
        rustup target add wasm32-wasip1 | Out-Null
        cargo build --release --target wasm32-wasip1 --manifest-path (Join-Path $src 'Cargo.toml')
        if ($LASTEXITCODE) { throw 'plugin build failed' }
        Copy-Item (Join-Path $src 'target\wasm32-wasip1\release\zj-sidebar.wasm') (Join-Path $work 'zj-sidebar.wasm')
        Say "building the helper ($Target)"
        cargo build --release --target $Target --manifest-path (Join-Path $src 'helper\Cargo.toml')
        if ($LASTEXITCODE) { throw 'helper build failed' }
        Copy-Item (Join-Path $src "helper\target\$Target\release\zjsb-helper.exe") (Join-Path $work 'zjsb-helper.exe')
    }
    else {
        if (-not $BaseUrl) {
            $BaseUrl = if ($Version -eq 'latest') { "https://github.com/$Repo/releases/latest/download" }
                       else { "https://github.com/$Repo/releases/download/$Version" }
        }
        Say "downloading from $BaseUrl"
        try {
            Invoke-WebRequest -UseBasicParsing "$BaseUrl/zj-sidebar.wasm" -OutFile (Join-Path $work 'zj-sidebar.wasm')
            Invoke-WebRequest -UseBasicParsing "$BaseUrl/zjsb-helper-$Target.exe" -OutFile (Join-Path $work 'zjsb-helper.exe')
        } catch { throw "download failed (no release yet? try -FromSource): $($_.Exception.Message)" }
        $sums = Join-Path $work 'SHA256SUMS'
        $haveSums = $true
        try { Invoke-WebRequest -UseBasicParsing "$BaseUrl/SHA256SUMS" -OutFile $sums } catch { $haveSums = $false }
        if ($haveSums) {
            foreach ($pair in @(@('zj-sidebar.wasm', 'zj-sidebar.wasm'), @('zjsb-helper.exe', "zjsb-helper-$Target.exe"))) {
                $want = (Get-Content $sums | ForEach-Object { $f = ($_ -split '\s+', 2); if ($f.Count -eq 2 -and $f[1].TrimStart('*') -eq $pair[1]) { $f[0] } } | Select-Object -First 1)
                $have = Get-Sha256 (Join-Path $work $pair[0])
                if (-not $want -or $want.ToLower() -ne $have.ToLower()) { throw "checksum mismatch for $($pair[1])" }
            }
            Say 'checksums ok'
        }
    }

    $plugins = Join-Path $ConfigDir 'plugins'
    $layouts = Join-Path $ConfigDir 'layouts'
    New-Item -ItemType Directory -Force $plugins, $layouts, $DataDir | Out-Null
    Copy-Item (Join-Path $work 'zj-sidebar.wasm') (Join-Path $plugins 'zj-sidebar.wasm') -Force
    Copy-Item (Join-Path $work 'zjsb-helper.exe') (Join-Path $DataDir 'zjsb-helper.exe') -Force

    $pluginUrl = 'file:' + ((Join-Path $plugins 'zj-sidebar.wasm') -replace '\\', '/')
    $layout = @"
// zj-sidebar: tab list on the left, grouped by directory. Generated by install.ps1.
layout {
    default_tab_template {
        pane split_direction="vertical" {
            pane size=32 borderless=true {
                plugin location="$pluginUrl"
            }
            pane
        }
        pane size=1 borderless=true {
            plugin location="zellij:status-bar"
        }
    }
}
"@
    # KDL doesn't care about the line endings; write LF and no BOM
    [IO.File]::WriteAllText((Join-Path $layouts 'sidebar.kdl'), ($layout -replace "`r`n", "`n") + "`n", (New-Object Text.UTF8Encoding $false))

    Say "installed: $plugins\zj-sidebar.wasm, $layouts\sidebar.kdl, $DataDir\zjsb-helper.exe"
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

if ($PatchProfile) {
    $profilePath = $PROFILE
    $existing = if (Test-Path $profilePath) { Get-Content $profilePath -Raw } else { '' }
    if ($existing -match '__zellijOrigPrompt|>>> zj-sidebar >>>') {
        Say "profile already has the zellij directory hook: $profilePath"
    } else {
        $block = @'

# >>> zj-sidebar >>>
# PowerShell's `cd` doesn't change the process working directory, which is what zellij
# reads for each pane. Sync it on every prompt (only inside zellij). Keep this at the
# end of the profile, after anything that defines `prompt`.
if ($env:ZELLIJ) {
    $global:__zellijOrigPrompt = $function:prompt
    function global:prompt {
        $loc = $ExecutionContext.SessionState.Path.CurrentFileSystemLocation
        if ($loc) { [Environment]::CurrentDirectory = $loc.ProviderPath }
        & $global:__zellijOrigPrompt
    }
}
# <<< zj-sidebar <<<
'@
        New-Item -ItemType Directory -Force (Split-Path $profilePath) | Out-Null
        Add-Content -Path $profilePath -Value $block
        Say "added the zellij directory hook to $profilePath"
    }
}

$pluginUrl = 'file:' + ((Join-Path $ConfigDir 'plugins\zj-sidebar.wasm') -replace '\\', '/')
Write-Host @"

Try it now:
    zellij --layout sidebar          (new session: zellij -n sidebar)

Make it the default: add this line to $ConfigDir\config.kdl
    default_layout "sidebar"

Optional keybinding (Alt+d toggles the git/sessions panel), inside
keybinds { shared_except "locked" { ... } } in config.kdl:
    bind "Alt d" { MessagePlugin "$pluginUrl" { name "toggle_panel"; }; }
(or just click "git / sessions" at the bottom of the left sidebar)

On first start, zellij asks the sidebar for permissions: click it and press y.
"@
if (-not $PatchProfile) {
    Write-Host @"

Tip: for pwsh, run this script with -PatchProfile (or see the README) so zellij can follow
`cd` in each pane; without it, tabs are grouped by the directory the shell started in.
"@
}
