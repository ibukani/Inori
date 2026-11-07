$ErrorActionPreference = 'Stop'
$runner = Get-Command python -ErrorAction SilentlyContinue
if (-not $runner) { $runner = Get-Command python3 -ErrorAction SilentlyContinue }
if (-not $runner) {
  Write-Error 'Python 3.11以上が必要です。https://www.python.org/downloads/ から導入し、ターミナルを開き直してください。'
  exit 1
}
& $runner.Source (Join-Path $PSScriptRoot 'tools/inori.py') @args
exit $LASTEXITCODE
