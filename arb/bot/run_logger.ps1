param(
    [double]$Hours = 0
)

# Prevent PowerShell from treating native stderr as terminating script errors
$ErrorActionPreference = "Continue"
if (Test-Path variable:global:PSNativeCommandUseErrorActionPreference) {
    $global:PSNativeCommandUseErrorActionPreference = $false
}

# Ensure we run in the bot directory
$BotDir = "C:\Users\user\Documents\Davis\Arbitrage\arb\bot"
Set-Location $BotDir

# Ensure data directory exists
if (-not (Test-Path "data")) {
    New-Item -ItemType Directory -Path "data" | Out-Null
}

# Archive existing opportunities.jsonl if present
if (Test-Path "data\opportunities.jsonl") {
    $archiveName = "data\old-" + (Get-Date -Format "yyyyMMdd-HHmm") + ".jsonl"
    Move-Item -Path "data\opportunities.jsonl" -Destination $archiveName
    Write-Host "[INFO] Archived existing opportunities.jsonl to $archiveName"
}

# Build release binary once before entering loop
Write-Host "[INFO] Building release binary..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    Write-Error "[ERROR] Build failed. Aborting."
    exit $LASTEXITCODE
}

# Resolve release binary path
$binPath = Join-Path $BotDir "target\x86_64-pc-windows-gnu\release\arb-bot.exe"
if (-not (Test-Path $binPath)) {
    $binPath = Join-Path $BotDir "target\release\arb-bot.exe"
}
if (-not (Test-Path $binPath)) {
    Write-Error "[ERROR] Compiled binary not found at $binPath"
    exit 1
}

$startTime = Get-Date
$endTime = if ($Hours -gt 0) { $startTime.AddHours($Hours) } else { $null }

if ($endTime) {
    Write-Host "[INFO] Running logger for $Hours hour(s) until $endTime (Press Ctrl+C to stop early)..."
} else {
    Write-Host "[INFO] Running logger continuously (Press Ctrl+C to stop)..."
}

while ($true) {
    if ($endTime -and (Get-Date) -ge $endTime) {
        Write-Host "[INFO] Reached time limit of $Hours hour(s). Exiting loop."
        break
    }

    $timestamp = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    Write-Host "[$timestamp] Starting arb-bot ($binPath)..."

    # Run compiled bot directly and pipe to log file without cargo stderr wrapping
    & $binPath 2>&1 | Tee-Object -FilePath "data\bot.log" -Append

    $exitTime = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    "[$exitTime] arb-bot exited, restarting in 5s..." | Tee-Object -FilePath "data\bot.log" -Append

    if ($endTime -and (Get-Date) -ge $endTime) {
        Write-Host "[INFO] Reached time limit of $Hours hour(s). Exiting loop."
        break
    }

    Start-Sleep -Seconds 5
}
