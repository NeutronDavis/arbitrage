Set-Location -Path "C:\Users\user\Documents\Davis\Arbitrage\arb\bot"

$hb = (Get-ChildItem data\scan_camelot_*.heartbeat.jsonl | Sort-Object LastWriteTime -Descending | Select-Object -First 1).FullName
$f = $hb -replace '\.heartbeat\.jsonl$',''

$out = & C:\scan2\summarize.exe $f --top 50 
$out                                                              # show it on screen
$out | Out-File -Encoding utf8 "data\summary_$(Get-Date -Format 'yyyyMMdd_HHmm').txt"
