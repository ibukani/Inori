#requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'

function Invoke-Inori {
    param([string]$Executable, [string[]]$Arguments)
    & $Executable @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Inori command failed with exit code $LASTEXITCODE"
    }
}

function Stop-InoriService {
    $service = Get-Service -Name 'InoriServer' -ErrorAction SilentlyContinue
    if ($null -ne $service -and $service.Status -ne 'Stopped') {
        Stop-Service -InputObject $service -Force -ErrorAction Stop
        $service.WaitForStatus([System.ServiceProcess.ServiceControllerStatus]::Stopped, [TimeSpan]::FromSeconds(15))
    }
}

$bundleDir = $PSScriptRoot
foreach ($line in Get-Content -LiteralPath (Join-Path $bundleDir 'SHA256SUMS')) {
    if ($line -notmatch '^([a-fA-F0-9]{64})  ([^\\/:]+)$') {
        throw 'Invalid SHA256SUMS entry.'
    }
    $expected = $Matches[1].ToLowerInvariant()
    $fileName = $Matches[2]
    $filePath = Join-Path $bundleDir $fileName
    if (-not (Test-Path -LiteralPath $filePath -PathType Leaf)) {
        throw "Missing package file: $fileName"
    }
    $actual = (Get-FileHash -LiteralPath $filePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "SHA256 mismatch: $fileName"
    }
}

$programFiles = [Environment]::GetFolderPath('ProgramFiles')
$programData = [Environment]::GetFolderPath('CommonApplicationData')
$binDir = Join-Path $programFiles 'Inori\server'
$dataDir = Join-Path $programData 'Inori\server'
$liveBin = Join-Path $binDir 'inori-server.exe'
$nextBin = Join-Path $binDir 'inori-server.new.exe'
$previousBin = Join-Path $binDir 'inori-server.previous.exe'
$rollbackDb = Join-Path $dataDir 'inori.sqlite.rollback'
$lockDir = Join-Path $binDir '.update-in-progress'
$backupDir = Join-Path $dataDir 'backups'
$backupFile = Join-Path $backupDir ("update-{0}-{1}.sqlite" -f [DateTime]::UtcNow.ToString('yyyyMMddTHHmmssZ'), $PID)
$packageBin = Join-Path $bundleDir 'inori-server.exe'

if (-not (Test-Path -LiteralPath $packageBin -PathType Leaf) -or
    -not (Test-Path -LiteralPath $liveBin -PathType Leaf) -or
    -not (Test-Path -LiteralPath (Join-Path $dataDir 'inori.sqlite') -PathType Leaf) -or
    -not (Test-Path -LiteralPath (Join-Path $dataDir 'server.toml') -PathType Leaf)) {
    throw 'Installed Inori server not found. Use setup.cmd for first installation.'
}
if ((Test-Path -LiteralPath $nextBin) -or (Test-Path -LiteralPath $previousBin) -or
    (Test-Path -LiteralPath $rollbackDb) -or (Test-Path -LiteralPath $lockDir)) {
    throw 'Update temporary files exist. Inspect the previous update before retrying.'
}

New-Item -ItemType Directory -Path $lockDir | Out-Null
$needsStart = $false
$committed = $false
try {
    Copy-Item -LiteralPath $packageBin -Destination $nextBin
    $needsStart = $true
    Invoke-Inori $liveBin @('--data-dir', $dataDir, 'service', 'stop')
    New-Item -ItemType Directory -Force -Path $backupDir | Out-Null
    Invoke-Inori $liveBin @('--data-dir', $dataDir, 'backup', $backupFile)
    Move-Item -LiteralPath $liveBin -Destination $previousBin
    Move-Item -LiteralPath $nextBin -Destination $liveBin
    Invoke-Inori $liveBin @('--data-dir', $dataDir, 'service', 'start')
    $committed = $true
}
catch {
    $failure = $_
    if ($needsStart) {
        Stop-InoriService
        if (Test-Path -LiteralPath $previousBin -PathType Leaf) {
            try {
                if (Test-Path -LiteralPath $liveBin) {
                    Remove-Item -LiteralPath $liveBin -Force
                }
                Move-Item -LiteralPath $previousBin -Destination $liveBin
                $restoreDir = Join-Path $dataDir ('.update-restore-' + $PID)
                New-Item -ItemType Directory -Path $restoreDir | Out-Null
                Invoke-Inori $liveBin @('--data-dir', $restoreDir, '--config', (Join-Path $dataDir 'server.toml'), 'restore', $backupFile)
                $restoredDb = Join-Path $restoreDir 'inori.sqlite'
                $rollbackTemp = Join-Path $dataDir 'inori.sqlite.rollback'
                Copy-Item -LiteralPath $restoredDb -Destination $rollbackTemp
                Remove-Item -LiteralPath (Join-Path $dataDir 'inori.sqlite-wal') -Force -ErrorAction SilentlyContinue
                Remove-Item -LiteralPath (Join-Path $dataDir 'inori.sqlite-shm') -Force -ErrorAction SilentlyContinue
                [System.IO.File]::Replace($rollbackTemp, (Join-Path $dataDir 'inori.sqlite'), $null)
                Remove-Item -LiteralPath $restoreDir -Recurse -Force
                Invoke-Inori $liveBin @('--data-dir', $dataDir, 'service', 'start')
                Write-Host 'Update failed; restored the previous binary and database.'
            }
            catch {
                Write-Error "Automatic rollback failed. Keep the service stopped and restore $backupFile manually. $_"
            }
        }
        else {
            try {
                Invoke-Inori $liveBin @('--data-dir', $dataDir, 'service', 'start')
            }
            catch {
                Write-Error "Could not restart the previous server. Database backup: $backupFile"
            }
        }
    }
    throw $failure
}
finally {
    Remove-Item -LiteralPath $nextBin -Force -ErrorAction SilentlyContinue
    if ($committed) {
        Remove-Item -LiteralPath $previousBin -Force -ErrorAction SilentlyContinue
        Write-Host "Update completed. Database backup: $backupFile"
    }
    Remove-Item -LiteralPath $lockDir -Force -ErrorAction SilentlyContinue
}
