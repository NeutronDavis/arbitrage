# Script to run independent Camelot scan in C:\scan2
Set-Location -Path "C:\Users\user\Documents\Davis\Arbitrage\arb\bot"

# 1. Prepare directory and copy binaries
New-Item -ItemType Directory -Force C:\scan2 | Out-Null
Copy-Item .\target\x86_64-pc-windows-gnu\release\arb-bot.exe C:\scan2\arb-bot.exe -Force
Copy-Item .\target\x86_64-pc-windows-gnu\release\summarize.exe C:\scan2\summarize.exe -Force

# 2. Run discovery writing only to data/markets_camelot.json (never touches data/markets.json)
& C:\scan2\arb-bot.exe --discover --markets-out data/markets_camelot.json

# 3. Configure environment and start resilient loop
$stamp = Get-Date -Format 'yyyyMMdd_HHmmss'
$env:MARKETS_FILE = "data/markets_camelot.json"
$env:OUTPUT_FILE  = "data/scan_camelot_$stamp.jsonl"
$env:LOG_MIN_SPREAD_BPS = "-2.0"
$log = "data/scan_camelot_$stamp.log"

Write-Host "[INFO] Starting scan with OUTPUT_FILE=$($env:OUTPUT_FILE)..."
while ($true) {
  & C:\scan2\arb-bot.exe 2>&1 | Tee-Object -FilePath $log -Append
  "exited ($LASTEXITCODE), restarting" | Tee-Object -FilePath $log -Append
  Start-Sleep -Seconds 5
}
