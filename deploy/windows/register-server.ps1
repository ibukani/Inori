param(
  [Parameter(Mandatory)][string]$Executable,
  [Parameter(Mandatory)][string]$DataDirectory,
  [Parameter(Mandatory)][string]$Config,
  [string]$Listen = '127.0.0.1:9473'
)
# Opt-in installer: run this script yourself after testing configuration and backups.
$serverExe = (Resolve-Path -LiteralPath $Executable -ErrorAction Stop).Path
$serverConfig = (Resolve-Path -LiteralPath $Config -ErrorAction Stop).Path
$serverData = [System.IO.Path]::GetFullPath($DataDirectory)
if ($serverExe -match '"' -or $serverConfig -match '"' -or $serverData -match '"') { throw 'Invalid quote in path' }
$arguments = "--data-dir `"$serverData`" --config `"$serverConfig`" --listen $Listen"
$action = New-ScheduledTaskAction -Execute $serverExe -Argument $arguments
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
Register-ScheduledTask -TaskName 'Inori Server' -Action $action -Trigger $trigger -Settings $settings -Description 'Private Inori character server, current-user logon' -ErrorAction Stop
