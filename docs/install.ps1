# calvin installer for Windows
# Usage: irm https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.ps1 | iex
#
# Optional environment variables:
#   CALVIN_VERSION          install this tag (e.g. v0.2.0) instead of the latest release
#   CALVIN_INSTALL_DIR      where to put calvin.exe (default: %LOCALAPPDATA%\calvin\bin)
#   CALVIN_INSTALL_ARCHIVE  install from a local .zip instead of downloading (used by CI)
#   CALVIN_NO_MODIFY_PATH   set to 1 to leave your PATH alone

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "jmelosegui/calvin"
$InstallDir = if ($env:CALVIN_INSTALL_DIR) { $env:CALVIN_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "calvin\bin" }
$Exe = Join-Path $InstallDir "calvin.exe"

function Write-Info { param($msg) Write-Host "==> " -ForegroundColor DarkYellow -NoNewline; Write-Host $msg }
# Throw rather than exit: under `irm | iex`, exit would close the user's PowerShell window.
function Write-Err { param($msg) throw "calvin install failed: $msg" }

Write-Info "Installing calvin..."

$Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($Arch -ne "AMD64") {
    Write-Err "There is no prebuilt calvin for Windows $Arch yet. Install from source: cargo install --git https://github.com/$Repo"
}
$Filename = "calvin-windows-amd64.zip"

$TempDir = Join-Path (Get-Item $env:TEMP).FullName "calvin-install-$PID"
New-Item -ItemType Directory -Force -Path $TempDir | Out-Null

try {
    if ($env:CALVIN_INSTALL_ARCHIVE) {
        $ZipPath = $env:CALVIN_INSTALL_ARCHIVE
        Write-Info "Using local archive $ZipPath"
    } else {
        $Headers = @{ "User-Agent" = "calvin-installer" }
        if ($env:CALVIN_VERSION) {
            $Version = $env:CALVIN_VERSION
        } else {
            try {
                $Version = (Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $Headers).tag_name
            } catch {
                Write-Err "Could not find the latest release. Check https://github.com/$Repo/releases"
            }
        }
        Write-Info "Version $Version"

        $Base = "https://github.com/$Repo/releases/download/$Version"
        $ZipPath = Join-Path $TempDir $Filename
        Write-Info "Downloading $Base/$Filename"
        Invoke-WebRequest -Uri "$Base/$Filename" -OutFile $ZipPath -Headers $Headers
        Invoke-WebRequest -Uri "$Base/SHA256SUMS" -OutFile (Join-Path $TempDir "SHA256SUMS") -Headers $Headers

        $Line = Get-Content (Join-Path $TempDir "SHA256SUMS") | Where-Object { $_ -match "\s\*?$([regex]::Escape($Filename))$" } | Select-Object -First 1
        if (-not $Line) { Write-Err "$Filename is missing from SHA256SUMS" }
        $Expected = ($Line -split "\s+")[0].ToLower()
        $Actual = (Get-FileHash -Algorithm SHA256 $ZipPath).Hash.ToLower()
        if ($Expected -ne $Actual) { Write-Err "Checksum mismatch for $Filename (expected $Expected, got $Actual)" }
        Write-Info "Checksum verified"
    }

    Expand-Archive -Path $ZipPath -DestinationPath (Join-Path $TempDir "x") -Force

    # A running calvin locks its executable: stop it cleanly, and start it again afterwards.
    $WasRunning = $false
    if (Test-Path $Exe) {
        $Status = & $Exe status 2>$null | Out-String
        if ($Status -match "is running") {
            Write-Info "Stopping the running calvin..."
            & $Exe stop | Out-Null
            $WasRunning = $true
        }
    }

    Write-Info "Installing to $InstallDir"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    # Retry briefly: Windows can hold the old executable for a moment after it exits.
    for ($i = 1; $i -le 10; $i++) {
        try { Copy-Item -Path (Join-Path $TempDir "x\calvin.exe") -Destination $Exe -Force; break }
        catch { if ($i -eq 10) { throw }; Start-Sleep -Milliseconds 500 }
    }
    Unblock-File -Path $Exe
} finally {
    Remove-Item -Recurse -Force $TempDir -ErrorAction SilentlyContinue
}

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if ($env:CALVIN_NO_MODIFY_PATH -ne "1" -and ($UserPath -split ";") -notcontains $InstallDir) {
    Write-Info "Adding $InstallDir to your PATH (open a new terminal to pick it up)"
    [Environment]::SetEnvironmentVariable("Path", (($UserPath.TrimEnd(";"), $InstallDir) -join ";"), "User")
}
$env:Path = "$env:Path;$InstallDir"

$Installed = & $Exe --version
Write-Info "Installed $Installed"

if ($WasRunning) {
    & $Exe start --no-open
} else {
    Write-Host ""
    Write-Host "Get started:  calvin start"
}
