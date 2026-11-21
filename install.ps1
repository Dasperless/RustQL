# RustQL Windows Installer
# https://github.com/Dasperless/RustQL
#
# Quick 1-Liner Install (Run in PowerShell / Windows Terminal):
#   irm https://raw.githubusercontent.com/Dasperless/RustQL/main/install.ps1 | iex
#
# Or with explicit execution policy bypass:
#   powershell -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/Dasperless/RustQL/main/install.ps1 | iex"

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallDir = ""
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = "Dasperless/RustQL"
$GitHubUrl = "https://github.com/$Repo"

function Write-Info($msg) {
    Write-Host "==> " -ForegroundColor Cyan -NoNewline
    Write-Host $msg -ForegroundColor White
}

function Write-Success($msg) {
    Write-Host "==> " -ForegroundColor Green -NoNewline
    Write-Host $msg -ForegroundColor White
}

function Write-WarnMsg($msg) {
    Write-Host "warning: " -ForegroundColor Yellow -NoNewline
    Write-Host $msg -ForegroundColor White
}

function Write-ErrMsg($msg) {
    Write-Host "error: " -ForegroundColor Red -NoNewline
    Write-Host $msg -ForegroundColor White
    exit 1
}

# 1. Architecture Check
if (-not [Environment]::Is64BitOperatingSystem) {
    Write-ErrMsg "RustQL prebuilt binaries require 64-bit Windows (x86_64). For 32-bit, please build from source using 'cargo build --release'."
}

# 2. Determine Version
if ([string]::IsNullOrWhiteSpace($Version)) {
    Write-Info "Resolving latest release for $Repo..."
    try {
        $LatestUrl = "https://api.github.com/repos/$Repo/releases/latest"
        $ReleaseInfo = Invoke-RestMethod -Uri $LatestUrl -UseBasicParsing -Headers @{ "User-Agent" = "RustQL-Installer" }
        if ($ReleaseInfo -and $ReleaseInfo.tag_name) {
            $Version = $ReleaseInfo.tag_name
        }
    } catch {
        # Fallback to base version if rate-limited or offline
        $Version = "v0.1.0"
    }
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $Version = "v0.1.0"
    }
}

if (-not $Version.StartsWith("v")) {
    $Version = "v$Version"
}

$Target = "x86_64-pc-windows-msvc"

Write-Info "Installing RustQL $Version ($Target)..."

# 3. Determine Installation Directory
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\rustql"
}

if (-not (Test-Path -Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# 4. Download Release Archive to Temp
$TempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("rustql_install_" + [System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $TempDir -Force | Out-Null

$CandidateArchives = @(
    "rustql-$Version-$Target.zip",
    "rustrix-$Version-$Target.zip"
)

$Downloaded = $false
$DownloadedArchive = ""
$ZipPath = ""

try {
    foreach ($ArchiveName in $CandidateArchives) {
        $DownloadUrl = "$GitHubUrl/releases/download/$Version/$ArchiveName"
        $CurrentZipPath = Join-Path $TempDir $ArchiveName
        try {
            Write-Info "Attempting download: $ArchiveName..."
            Invoke-WebRequest -Uri $DownloadUrl -OutFile $CurrentZipPath -UseBasicParsing
            $Downloaded = $true
            $DownloadedArchive = $ArchiveName
            $ZipPath = $CurrentZipPath
            break
        } catch {
            # Try next candidate
        }
    }

    if (-not $Downloaded) {
        Write-ErrMsg "Failed to download release archive for $Version from $GitHubUrl/releases."
    }

    # 5. Prevent Windows SmartScreen / Mark-of-the-Web Warnings: Unblock archive
    if (Get-Command -Name Unblock-File -ErrorAction SilentlyContinue) {
        Unblock-File -Path $ZipPath
    }

    # Verify SHA256 if available
    $ShaUrl = "$GitHubUrl/releases/download/$Version/$DownloadedArchive.sha256"
    try {
        $ExpectedShaRaw = (Invoke-WebRequest -Uri $ShaUrl -UseBasicParsing).Content.Trim()
        $ExpectedHash = ($ExpectedShaRaw -split '\s+')[0].ToLower()
        $ActualHash = (Get-FileHash -Algorithm SHA256 -Path $ZipPath).Hash.ToLower()
        if ($ExpectedHash -and ($ActualHash -ne $ExpectedHash)) {
            Write-ErrMsg "SHA256 checksum mismatch! Expected: $ExpectedHash, Got: $ActualHash"
        }
        Write-Info "SHA256 checksum verified ($ExpectedHash)."
    } catch {
        # SHA verification is best-effort
    }

    # 6. Extract Archive
    Write-Info "Extracting binary..."
    Expand-Archive -Path $ZipPath -DestinationPath $TempDir -Force

    $ExtractedBin = $null
    $BinCandidates = @(
        (Join-Path $TempDir "rustql-$Version-$Target\rustql.exe"),
        (Join-Path $TempDir "rustrix-$Version-$Target\rustrix.exe"),
        (Join-Path $TempDir "rustql.exe"),
        (Join-Path $TempDir "rustrix.exe")
    )
    foreach ($cand in $BinCandidates) {
        if (Test-Path -Path $cand) {
            $ExtractedBin = $cand
            break
        }
    }
    if (-not $ExtractedBin) {
        $found = Get-ChildItem -Path $TempDir -Recurse -Filter "*.exe" -ErrorAction SilentlyContinue | Where-Object { $_.Name -match "^rust(ql|rix)\.exe$" } | Select-Object -First 1
        if ($found) {
            $ExtractedBin = $found.FullName
        }
    }

    if (-not $ExtractedBin) {
        Write-ErrMsg "Could not find rustql.exe or rustrix.exe in extracted archive."
    }

    # 7. Copy to Target Directory and Unblock
    $DestExe = Join-Path $InstallDir "rustql.exe"
    Copy-Item -Path $ExtractedBin -Destination $DestExe -Force

    # Unblock final binary to guarantee zero SmartScreen / antivirus warning dialogs
    if (Get-Command -Name Unblock-File -ErrorAction SilentlyContinue) {
        Unblock-File -Path $DestExe
    }

    # 8. Add to User PATH if not already present
    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -split ';' -notcontains $InstallDir) {
        Write-Info "Adding $InstallDir to User PATH..."
        $NewUserPath = if ([string]::IsNullOrWhiteSpace($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    }

    # Update current session PATH so command works immediately
    if ($env:Path -split ';' -notcontains $InstallDir) {
        $env:Path = "$InstallDir;$env:Path"
    }

    Write-Success "RustQL successfully installed to: $DestExe"

    # 9. Verify Installation
    try {
        & $DestExe --version
    } catch {
        Write-WarnMsg "Installed successfully, but could not execute '$DestExe --version'."
    }

    Write-Host ""
    Write-Host "Quick Start on Windows:" -ForegroundColor Cyan
    Write-Host "  Interactive TUI (Windows Terminal recommended):" -ForegroundColor White
    Write-Host "    rustql examples\users.json examples\orders.ndjson" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Run SQL in PowerShell (use single quotes for SQL):" -ForegroundColor White
    Write-Host "    rustql -q 'SELECT id, name, department FROM users WHERE active = true' examples\users.json" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Documentation: https://github.com/Dasperless/RustQL#readme" -ForegroundColor Gray
} finally {
    if (Test-Path -Path $TempDir) {
        Remove-Item -Path $TempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
