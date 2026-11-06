param(
  [Parameter(Mandatory)][int[]]$ProcessIds,
  [ValidateRange(2,86400)][int]$Seconds = 30,
  [ValidateSet('visible','minimized','background')][string]$Phase = 'visible',
  [string]$Output = 'artifacts/performance/processes.json'
)
$records = [System.Collections.Generic.List[object]]::new()
$previous = @{}
$start = [DateTimeOffset]::UtcNow
$gpuAvailable = $true
while (([DateTimeOffset]::UtcNow - $start).TotalSeconds -lt $Seconds) {
  $now = [DateTimeOffset]::UtcNow
  $gpu = @()
  if ($gpuAvailable) {
    try { $gpu = (Get-Counter '\GPU Process Memory(*)\Dedicated Usage','\GPU Process Memory(*)\Shared Usage' -ErrorAction Stop).CounterSamples }
    catch { $gpuAvailable = $false }
  }
  foreach ($processId in $ProcessIds) {
    $process = Get-Process -Id $processId -ErrorAction SilentlyContinue
    if ($null -eq $process) { continue }
    $cpuSeconds = $process.TotalProcessorTime.TotalSeconds
    $cpu = $null
    if ($previous.ContainsKey($processId)) {
      $old = $previous[$processId]
      $cpu = [Math]::Round(100 * ($cpuSeconds - $old.Cpu) / ($now - $old.Time).TotalSeconds, 3)
    }
    $previous[$processId] = @{Cpu=$cpuSeconds;Time=$now}
    $ownGpu = @($gpu | Where-Object { $_.InstanceName -match "^pid_${processId}_" })
    $dedicated = $null; $shared = $null
    if ($gpuAvailable) {
      $dedicated = ($ownGpu | Where-Object Path -Match 'Dedicated Usage' | Measure-Object CookedValue -Sum).Sum
      $shared = ($ownGpu | Where-Object Path -Match 'Shared Usage' | Measure-Object CookedValue -Sum).Sum
    }
    $records.Add([pscustomobject]@{ElapsedSeconds=[Math]::Round(($now-$start).TotalSeconds,2);Name=$process.ProcessName;ProcessId=$processId;WorkingSetMB=[Math]::Round($process.WorkingSet64/1MB,2);PrivateBytesMB=[Math]::Round($process.PrivateMemorySize64/1MB,2);CpuPercentOfOneCore=$cpu;GpuDedicatedBytes=$dedicated;GpuSharedBytes=$shared})
  }
  Start-Sleep -Milliseconds 1000
}
$destination = [System.IO.Path]::GetFullPath($Output)
New-Item -ItemType Directory -Path ([System.IO.Path]::GetDirectoryName($destination)) -Force | Out-Null
@{Phase=$Phase;RequestedSeconds=$Seconds;ActualSeconds=([DateTimeOffset]::UtcNow-$start).TotalSeconds;GpuCountersAvailable=$gpuAvailable;Samples=$records} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $destination -Encoding utf8
Write-Output "Measurements saved: $destination"
