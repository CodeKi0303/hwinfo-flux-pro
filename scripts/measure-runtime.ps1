#Requires -Version 7.0
<#
.SYNOPSIS
Measure a real display run, then request a cooperative stop of that exact session.
.DESCRIPTION
Sends real HID reports and stops the owned session using its exact instance token.
A replacement session ignores that token. The public stop command targets the
current session and requires separate validation.

Samples are streamed every 500 ms; only aggregate counters remain in memory.
Startup means the first five seconds of this launch, not a guaranteed cold OS
cache. Working-set peak is the OS lifetime high-water mark. Private-memory peaks
are sampled maxima. HWiNFO and the PowerShell observer are excluded. Successful
USB writes do not establish that physical digits or the missing marker rendered.
.EXAMPLE
pwsh -File scripts/measure-runtime.ps1 -Executable .\target\x86_64-pc-windows-msvc\release\hwinfo-flux-pro.exe -Config "$env:LOCALAPPDATA\hwinfo-flux-pro\config.json" -OutputDirectory .\.local\measurements\run-01 -Seconds 30
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)] [string] $Executable,
    [Parameter(Mandatory)] [string] $Config,
    [Parameter(Mandatory)] [string] $OutputDirectory,
    [ValidateRange(10, 300)] [int] $Seconds = 30
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'This harness requires Windows and PowerShell 7.' }
$encoding = [Text.UTF8Encoding]::new($false, $true)
$executablePath = (Resolve-Path -LiteralPath $Executable).ProviderPath
$configPath = (Resolve-Path -LiteralPath $Config).ProviderPath
$outputPath = [IO.Path]::GetFullPath($OutputDirectory)
if (-not [IO.File]::Exists($executablePath)) { throw 'Executable must be a file.' }
if ([IO.File]::Exists($outputPath) -or [IO.Directory]::Exists($outputPath)) {
    throw "OutputDirectory already exists and will be preserved: $outputPath"
}
if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) { throw 'LOCALAPPDATA is unavailable.' }
$statePath = Join-Path $env:LOCALAPPDATA 'hwinfo-flux-pro'

function Read-SmallJson([string] $Path) {
    $stream = $null
    try {
        # FILE_SHARE_DELETE avoids interfering with the application's atomic rename.
        $stream = [IO.FileStream]::new($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read,
            ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
        $buffer = [byte[]]::new(65537)
        $length = 0
        while ($length -lt $buffer.Length) {
            $read = $stream.Read($buffer, $length, $buffer.Length - $length)
            if ($read -eq 0) { break }
            $length += $read
        }
        if ($length -gt 65536) { throw 'JSON file exceeds 64 KiB.' }
        return ($encoding.GetString($buffer, 0, $length) | ConvertFrom-Json -AsHashtable)
    } finally {
        if ($null -ne $stream) { $stream.Dispose() }
    }
}

function Write-JsonLine([IO.StreamWriter] $Writer, $Value) {
    $Writer.WriteLine(($Value | ConvertTo-Json -Depth 16 -Compress))
    $Writer.Flush()
}

function New-Capture([string] $Label, [string[]] $Arguments) {
    $stdoutPath = Join-Path $outputPath "$Label.stdout.txt"
    $stderrPath = Join-Path $outputPath "$Label.stderr.txt"
    $stdoutFile = [IO.FileStream]::new($stdoutPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
    $stderrFile = $null
    $process = [Diagnostics.Process]::new()
    try {
        $stderrFile = [IO.FileStream]::new($stderrPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::Read)
        $start = [Diagnostics.ProcessStartInfo]::new()
        $start.FileName = $executablePath
        $start.UseShellExecute = $false
        $start.CreateNoWindow = $true
        $start.WindowStyle = [Diagnostics.ProcessWindowStyle]::Hidden
        $start.RedirectStandardInput = $true
        $start.RedirectStandardOutput = $true
        $start.RedirectStandardError = $true
        foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
        $process.StartInfo = $start
        if (-not $process.Start()) { throw 'Process.Start returned false.' }
        # Preserve the actual OS process handle, never reacquire by PID.
        [void] $process.Handle
        $process.StandardInput.Close()
        return [pscustomobject]@{
            Process = $process
            StdoutPath = $stdoutPath
            StderrPath = $stderrPath
            StdoutFile = $stdoutFile
            StderrFile = $stderrFile
            StdoutCopy = $process.StandardOutput.BaseStream.CopyToAsync($stdoutFile, 4096)
            StderrCopy = $process.StandardError.BaseStream.CopyToAsync($stderrFile, 4096)
        }
    } catch {
        # Closing stream handles does not terminate a started process.
        $stdoutFile.Dispose()
        if ($null -ne $stderrFile) { $stderrFile.Dispose() }
        $process.Dispose()
        throw
    }
}

function Complete-Capture($Capture) {
    if (-not $Capture.Process.HasExited) { return $false }
    $copies = [Threading.Tasks.Task]::WhenAll([Threading.Tasks.Task[]]@($Capture.StdoutCopy, $Capture.StderrCopy))
    if (-not $copies.Wait(5000)) { throw 'Output draining timed out after process exit.' }
    $Capture.StdoutFile.Flush()
    $Capture.StderrFile.Flush()
    $Capture.StdoutFile.Dispose()
    $Capture.StderrFile.Dispose()
    return $true
}

function New-Aggregate {
    return @{ count = 0; working_set_sum = 0.0; working_set_min = [long]::MaxValue;
        working_set_max = 0L; private_min = [long]::MaxValue; private_max = 0L }
}

function Add-Aggregate($Aggregate, [long] $WorkingSet, [long] $PrivateBytes) {
    $Aggregate.count++
    $Aggregate.working_set_sum += $WorkingSet
    $Aggregate.working_set_min = [Math]::Min($Aggregate.working_set_min, $WorkingSet)
    $Aggregate.working_set_max = [Math]::Max($Aggregate.working_set_max, $WorkingSet)
    $Aggregate.private_min = [Math]::Min($Aggregate.private_min, $PrivateBytes)
    $Aggregate.private_max = [Math]::Max($Aggregate.private_max, $PrivateBytes)
}

function Summarize-Aggregate($Aggregate) {
    if ($Aggregate.count -eq 0) { return @{ sample_count = 0 } }
    return @{ sample_count = $Aggregate.count; working_set_bytes_min = $Aggregate.working_set_min;
        working_set_bytes_mean = [Math]::Round($Aggregate.working_set_sum / $Aggregate.count);
        working_set_bytes_max_sampled = $Aggregate.working_set_max;
        private_bytes_min = $Aggregate.private_min; private_bytes_max_sampled = $Aggregate.private_max }
}

$configuration = Read-SmallJson $configPath
$interval = [long] $configuration.interval_ms
if ($interval -lt 1000 -or $interval -gt 60000) { throw 'Config interval_ms must be 1000..60000.' }
# Run counts frames at the configured display interval.
# If the observer fails, the child still exits normally after a bounded count.
$fallbackSamples = $Seconds + 11
[void] [IO.Directory]::CreateDirectory($outputPath)
$issues = [Collections.Generic.List[string]]::new()
$captures = [Collections.Generic.List[object]]::new()
$run = $null
$samplesWriter = $null
$session = $null
$ownedPid = $null
$ownedStartUnixMs = $null
$stopRequested = $false
$stopwatch = [Diagnostics.Stopwatch]::new()
$startup = New-Aggregate
$steady = New-Aggregate
$osPeakWorkingSet = 0L
$cpuAtLastSample = 0.0
$liveDisplayingSamples = 0
$ownedStateSamples = 0
$measurementSamples = 0
$finalState = $null
$finalCliState = $null
$exitCode = $null
$elapsed = 0.0
$processWallSeconds = $null
$startedUtc = [DateTimeOffset]::UtcNow.ToString('o')
$binaryHash = (Get-FileHash -LiteralPath $executablePath -Algorithm SHA256).Hash
$binaryBytes = (Get-Item -LiteralPath $executablePath).Length

try {
    $preflight = New-Capture 'preflight-status' @('status')
    $captures.Add($preflight)
    if (-not $preflight.Process.WaitForExit(10000)) { throw 'Preflight status timed out; no measurement run was launched.' }
    [void] (Complete-Capture $preflight)
    if ($preflight.Process.ExitCode -ne 0) { throw 'Preflight status failed; inspect preflight-status.stderr.txt.' }
    $before = Read-SmallJson $preflight.StdoutPath
    if ($before.instance_lock_held -isnot [bool]) { throw 'Preflight returned an unknown instance-lock state.' }
    if ($before.instance_lock_held) { throw 'An instance is already running; no measurement run was launched.' }

    $samplesWriter = [IO.StreamWriter]::new((Join-Path $outputPath 'samples.jsonl'), $false, $encoding, 4096)
    $stopwatch.Start()
    $run = New-Capture 'run' @('run', '--config', $configPath, '--samples', $fallbackSamples.ToString())
    $captures.Add($run)
    $ownedPid = $run.Process.Id
    $ownedStartUnixMs = ([DateTimeOffset] $run.Process.StartTime.ToUniversalTime()).ToUnixTimeMilliseconds()
    $maximumSamples = $Seconds * 2 + 1
    while ($stopwatch.Elapsed.TotalSeconds -lt $Seconds -and $measurementSamples -lt $maximumSamples) {
        if ($run.Process.HasExited) { break }
        $iteration = [Diagnostics.Stopwatch]::StartNew()
        $run.Process.Refresh()
        $elapsed = $stopwatch.Elapsed.TotalSeconds
        $workingSet = $run.Process.WorkingSet64
        $privateBytes = $run.Process.PrivateMemorySize64
        $osPeakWorkingSet = [Math]::Max($osPeakWorkingSet, $run.Process.PeakWorkingSet64)
        $cpuAtLastSample = $run.Process.TotalProcessorTime.TotalSeconds
        $bucket = if ($elapsed -lt 5) { $startup } else { $steady }
        Add-Aggregate $bucket $workingSet $privateBytes
        $measurementSamples++

        $state = $null
        $stateError = $null
        try {
            $instance = Read-SmallJson (Join-Path $statePath 'instance.json')
            if ($instance.pid -ne $ownedPid -or [decimal] $instance.started_unix_ms -lt $ownedStartUnixMs) {
                throw 'Instance identity does not match the exact child launch.'
            }
            if ($null -eq $session) {
                if ([string] $instance.session -notmatch ('^' + $ownedPid + '-[0-9]{1,30}$')) { throw 'Instance session token has an unexpected format.' }
                $session = [string] $instance.session
            }
            if ([string]::IsNullOrEmpty($session) -or $instance.session -cne $session) { throw 'The captured instance session changed.' }
            $candidate = Read-SmallJson (Join-Path $statePath 'status.json')
            if ($candidate.pid -ne $ownedPid -or $candidate.session -cne $session) { throw 'Status belongs to another instance/session.' }
            $state = $candidate
            $ownedStateSamples++
            if ($state.phase -eq 'displaying' -and -not $state.dry_run -and $null -eq $state.output_error) { $liveDisplayingSamples++ }
        } catch { $stateError = $_.Exception.Message }
        Write-JsonLine $samplesWriter @{ elapsed_seconds = [Math]::Round($elapsed, 4);
            pid = $ownedPid; session = $session; bucket = $(if ($elapsed -lt 5) { 'startup' } else { 'steady' });
            working_set_bytes = $workingSet; private_bytes = $privateBytes;
            lifetime_peak_working_set_bytes = $osPeakWorkingSet; cpu_seconds = $cpuAtLastSample;
            status = $state; status_error = $stateError }
        $remaining = 500 - [int] $iteration.ElapsedMilliseconds
        if ($remaining -gt 0) { Start-Sleep -Milliseconds $remaining }
    }
} catch { $issues.Add($_.Exception.Message) }
finally {
    if ($null -ne $run) {
        try {
            if (-not $run.Process.HasExited) {
                if ([string]::IsNullOrEmpty($session)) { throw 'No owned session was captured; bounded child fallback is left running.' }
                $instance = Read-SmallJson (Join-Path $statePath 'instance.json')
                if ($instance.pid -ne $ownedPid -or $instance.session -cne $session) { throw 'Current instance is not the captured session; no stop request was written.' }
                # Protocol writes contain no newline/BOM. A future session ignores this token.
                [IO.File]::WriteAllText((Join-Path $statePath 'stop.request'), $session, $encoding)
                $stopRequested = $true
                if (-not $run.Process.WaitForExit(10000)) { throw 'Cooperative stop timed out; child was not killed and display cleanup is unconfirmed.' }
            }
            $elapsed = $stopwatch.Elapsed.TotalSeconds
            $exitCode = $run.Process.ExitCode
            $processWallSeconds = ($run.Process.ExitTime.ToUniversalTime() - $run.Process.StartTime.ToUniversalTime()).TotalSeconds
            $cpuAtLastSample = $run.Process.TotalProcessorTime.TotalSeconds
            [void] (Complete-Capture $run)
            $finalState = Read-SmallJson (Join-Path $statePath 'status.json')
            if ($finalState.pid -ne $ownedPid -or $finalState.session -cne $session) { throw 'Final status does not belong to the captured session.' }
            if ($exitCode -ne 0) { $issues.Add("Child exited with code $exitCode; inspect run.stderr.txt.") }
            if ($finalState.phase -ne 'stopped' -or -not $finalState.display_cleared -or
                $null -ne $finalState.clear_error -or $null -ne $finalState.run_error) {
                $issues.Add('Final status does not confirm a clean stop and successful display-clear write.')
            }
        } catch { $issues.Add($_.Exception.Message) }
        if ($run.Process.HasExited) {
            try {
                $after = New-Capture 'final-status' @('status')
                $captures.Add($after)
                if (-not $after.Process.WaitForExit(10000)) { throw 'Final status command timed out.' }
                [void] (Complete-Capture $after)
                if ($after.Process.ExitCode -ne 0) { throw 'Final status command failed.' }
                $finalCliState = Read-SmallJson $after.StdoutPath
                if ($finalCliState.instance_lock_held -isnot [bool] -or $finalCliState.instance_lock_held) {
                    $issues.Add('Final instance lock is held or unknown; a replacement instance may have started.')
                }
            } catch { $issues.Add($_.Exception.Message) }
        }
    }
    $stopwatch.Stop()
    if ($null -ne $samplesWriter) { $samplesWriter.Dispose() }
}

if ($null -ne $run -and $liveDisplayingSamples -eq 0) { $issues.Add('No owned live displaying status was observed.') }
$summary = [ordered]@{
    schema_version = 1; started_utc = $startedUtc; executable = $executablePath;
    executable_sha256 = $binaryHash; executable_bytes = $binaryBytes; config = $configPath;
    requested_seconds = $Seconds; fallback_sample_limit = $fallbackSamples;
    owned_pid = $ownedPid; owned_process_start_unix_ms = $ownedStartUnixMs; session = $session;
    observer_wall_seconds = [Math]::Round($stopwatch.Elapsed.TotalSeconds, 4);
    process_lifetime_wall_seconds = $processWallSeconds;
    cpu_seconds = $cpuAtLastSample;
    cpu_percent_one_core = $(if ($null -ne $processWallSeconds -and $processWallSeconds -gt 0) { [Math]::Round(100 * $cpuAtLastSample / $processWallSeconds, 4) } else { $null });
    lifetime_peak_working_set_bytes_observed = $osPeakWorkingSet;
    startup_first_five_seconds = (Summarize-Aggregate $startup);
    steady_after_five_seconds = (Summarize-Aggregate $steady);
    owned_status_samples = $ownedStateSamples; displaying_status_samples = $liveDisplayingSamples;
    cooperative_session_stop_requested = $stopRequested; child_exit_code = $exitCode;
    child_still_running = $(if ($null -ne $run) { -not $run.Process.HasExited } else { $false });
    final_status = $finalState; final_cli_status = $finalCliState;
    physical_display_verified = $false;
    limitations = @('Only the bridge child is measured; HWiNFO and this observer are excluded.',
        'Startup and steady windows do not establish cold/warm OS caches.',
        'Private-memory maxima are sampled, not an OS lifetime peak.',
        'Gadget has no measurement timestamp; accepted USB writes do not prove visible display rendering.',
        'This uses exact-session stop.request; the public stop CLI is a separate verification.');
    errors = $issues.ToArray()
}
[IO.File]::WriteAllText((Join-Path $outputPath 'summary.json'), ($summary | ConvertTo-Json -Depth 20), $encoding)
foreach ($capture in $captures) {
    if ($capture.Process.HasExited) { $capture.Process.Dispose() }
}
Write-Host "Measurement records: $outputPath"
if ($issues.Count -gt 0) { throw ($issues -join "`n") }
