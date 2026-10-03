[CmdletBinding()]
param(
    [ValidateSet('SelfTest', 'DryRun', 'Official', 'ExportPartial')]
    [string]$Mode = 'SelfTest',
    [string]$InputRawPath,
    [string]$CsvPath,
    [string]$OutputDirectory,
    [string]$RuntimeRoot = (Join-Path $env:USERPROFILE '.emutrim-benchmark-beta-37211\profile\.emutrim-benchmark-080'),
    [string]$AndroidCli = (Join-Path $env:LOCALAPPDATA 'AndroidCLI\android.exe'),
    [string]$JavaHome = $env:JAVA_HOME,
    [string]$ExternalSdkRoot = (Join-Path $env:LOCALAPPDATA 'Android\Sdk')
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$script:Repo = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$script:BenchmarkRoot = Join-Path $env:TEMP 'emutrim-benchmark-080'
$script:RuntimeRoot = $RuntimeRoot
$script:ManagedRoot = Join-Path $script:RuntimeRoot 'managed'
$script:ManagedSdk = Join-Path $script:ManagedRoot 'sdk'
$script:BuildSdk = Join-Path $script:BenchmarkRoot 'build-sdk'
$script:Fixture = Join-Path $script:BenchmarkRoot 'testing-samples'
$script:FixtureProject = Join-Path $script:Fixture 'ui\espresso\BasicSample'
$script:FixtureLockPath = Join-Path $script:Repo 'tools\benchmark\fixture.lock.json'
$script:EmuTrim = Join-Path $script:Repo 'target\release\emutrim.exe'
$script:AvdName = 'EmuTrim_Managed'
$script:ApplicationId = 'com.example.android.testing.espresso.BasicSample'
$script:MainActivity = 'com.example.android.testing.espresso.BasicSample.MainActivity'
$script:AndroidCli = $AndroidCli
$script:JavaHome = $JavaHome
$script:ExternalSdkRoot = $ExternalSdkRoot
$script:PowerShell = Join-Path $PSHOME 'pwsh.exe'
$script:HarnessPath = $PSCommandPath
$script:OfficialBaselineApproved = $false

Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Threading;

public sealed class ProcessPipeCapture {
    private readonly Process process;
    private readonly StringBuilder stdout = new StringBuilder();
    private readonly StringBuilder stderr = new StringBuilder();
    private readonly Thread stdoutThread;
    private readonly Thread stderrThread;

    public ProcessPipeCapture(Process process) {
        this.process = process;
        stdoutThread = new Thread(() => Copy(process.StandardOutput, stdout)) { IsBackground = true };
        stderrThread = new Thread(() => Copy(process.StandardError, stderr)) { IsBackground = true };
        stdoutThread.Start();
        stderrThread.Start();
    }

    private static void Copy(TextReader reader, StringBuilder target) {
        var buffer = new char[4096];
        try {
            int count;
            while ((count = reader.Read(buffer, 0, buffer.Length)) > 0) lock (target) target.Append(buffer, 0, count);
        } catch (IOException) { } catch (ObjectDisposedException) { }
    }

    public string Stdout { get { lock (stdout) return stdout.ToString(); } }
    public string Stderr { get { lock (stderr) return stderr.ToString(); } }
    public void Stop() {
        try { process.StandardOutput.Close(); } catch { }
        try { process.StandardError.Close(); } catch { }
        stdoutThread.Join(500);
        stderrThread.Join(500);
    }
}
'@

function Parse-SecondsMilliseconds([string]$Value) {
    if ($Value -match '^\s*(?<seconds>[0-9]+(?:\.[0-9]+)?)s\s*$') {
        return [long][math]::Round(([double]$Matches.seconds) * 1000)
    }
    return $null
}

function Parse-StartupTiming([string]$Text) {
    $line = [regex]::Matches($Text, '(?m)^startup timing:.*$') | Select-Object -Last 1
    if ($null -eq $line) { return $null }
    $raw = $line.Value
    $names = [ordered]@{
        launch_to_console_ms = 'launch→console'
        console_to_adb_ms = 'console→ADB'
        adb_to_device_ms = 'ADB→device'
        device_to_boot_ms = 'device→boot'
        boot_to_ready_ms = 'boot→ready'
        total_ms = 'total'
    }
    $parsed = [ordered]@{ raw = $raw }
    foreach ($key in $names.Keys) {
        $name = [regex]::Escape($names[$key])
        $match = [regex]::Match($raw, "${name}\s+([^;]+)")
        $parsed[$key] = if ($match.Success) { Parse-SecondsMilliseconds $match.Groups[1].Value } else { $null }
    }
    if ($null -ne $parsed.launch_to_console_ms -and $null -ne $parsed.console_to_adb_ms -and
        $null -ne $parsed.adb_to_device_ms -and $null -ne $parsed.device_to_boot_ms) {
        $parsed.boot_complete_ms_from_phases = [long]($parsed.launch_to_console_ms + $parsed.console_to_adb_ms + $parsed.adb_to_device_ms + $parsed.device_to_boot_ms)
    } else {
        $parsed.boot_complete_ms_from_phases = $null
    }
    return [pscustomobject]$parsed
}

function Parse-ActivityLaunch([string]$Text, [long]$HostElapsedMs) {
    $result = [ordered]@{
        host_elapsed_ms = $HostElapsedMs
        status = $null
        activity = $null
        this_time_ms = $null
        total_time_ms = $null
        wait_time_ms = $null
        complete = $false
        raw_output = $Text.Trim()
    }
    foreach ($field in @(@('status', 'Status'), @('activity', 'Activity'))) {
        $m = [regex]::Match($Text, "(?m)^\s*$($field[1]):\s*(.+?)\s*$")
        if ($m.Success) { $result[$field[0]] = $m.Groups[1].Value }
    }
    foreach ($field in @(@('this_time_ms', 'ThisTime'), @('total_time_ms', 'TotalTime'), @('wait_time_ms', 'WaitTime'))) {
        $m = [regex]::Match($Text, "(?m)^\s*$($field[1]):\s*(\d+)\s*$")
        if ($m.Success) { $result[$field[0]] = [long]$m.Groups[1].Value }
    }
    $result.complete = [regex]::IsMatch($Text, '(?m)^\s*Complete\s*$')
    return [pscustomobject]$result
}

function Get-Median([double[]]$Values) {
    if ($Values.Count -eq 0) { return $null }
    $sorted = @($Values | Sort-Object)
    $middle = [int][math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 1) { return [double]$sorted[$middle] }
    return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2
}

function Get-Statistics([object[]]$Values) {
    $numbers = @($Values | Where-Object { $null -ne $_ } | ForEach-Object { [double]$_ })
    if ($numbers.Count -eq 0) {
        return [pscustomobject]@{ n = 0; raw = @(); median = $null; mean = $null; min = $null; max = $null }
    }
    return [pscustomobject]@{
        n = $numbers.Count
        raw = $numbers
        median = Get-Median $numbers
        mean = ($numbers | Measure-Object -Average).Average
        min = ($numbers | Measure-Object -Minimum).Minimum
        max = ($numbers | Measure-Object -Maximum).Maximum
    }
}

function Get-ConditionOrder {
    return @(
        [pscustomobject]@{ pair = 1; condition = 'CONTROL'; order = 1 },
        [pscustomobject]@{ pair = 1; condition = 'SLIM'; order = 2 },
        [pscustomobject]@{ pair = 2; condition = 'SLIM'; order = 1 },
        [pscustomobject]@{ pair = 2; condition = 'CONTROL'; order = 2 },
        [pscustomobject]@{ pair = 3; condition = 'CONTROL'; order = 1 },
        [pscustomobject]@{ pair = 3; condition = 'SLIM'; order = 2 },
        [pscustomobject]@{ pair = 4; condition = 'SLIM'; order = 1 },
        [pscustomobject]@{ pair = 4; condition = 'CONTROL'; order = 2 },
        [pscustomobject]@{ pair = 5; condition = 'CONTROL'; order = 1 },
        [pscustomobject]@{ pair = 5; condition = 'SLIM'; order = 2 }
    )
}

function Add-Failure($Record, [string]$Phase, [string]$ErrorText, $ExitCode, $ElapsedMs) {
    $Record.failures += [pscustomobject]@{
        condition = $Record.condition
        pair = $Record.pair
        trial = $Record.trial
        phase = $Phase
        exit_code = $ExitCode
        error = $ErrorText
        elapsed_ms = $ElapsedMs
    }
}

function Get-Excerpt([string]$Text, [int]$MaxLines = 35) {
    if ([string]::IsNullOrWhiteSpace($Text)) { return '' }
    return (($Text -split "`r?`n" | Select-Object -Last $MaxLines) -join "`n").Trim()
}

function Get-IdleMedian($Trial, [string]$Property) {
    if ($null -eq $Trial.idle -or $null -eq $Trial.idle.samples -or $Trial.idle.samples.Count -eq 0) { return $null }
    return Get-Median ([double[]]@($Trial.idle.samples | Where-Object { $null -ne $_.$Property } | ForEach-Object { [double]$_.$Property }))
}

function Get-OptionalMetric($Object, [string]$Path) {
    if ($null -eq $Object) { return $null }
    $value = $Object
    foreach ($segment in $Path.Split('.')) {
        if ($null -eq $value) { return $null }
        $property = $value.PSObject.Properties[$segment]
        if ($null -eq $property) { throw "Unexpected trial schema: missing '$segment' in '$Path'" }
        $value = $property.Value
        if ($null -eq $value) { return $null }
    }
    return $value
}

function Get-MetricValue($Trial, [string]$Metric) {
    switch ($Metric) {
        'boot_complete_ms' { return Get-OptionalMetric $Trial 'start.boot_complete_ms' }
        'start_total_ms' { return Get-OptionalMetric $Trial 'start.command_wall_ms' }
        'slim_overhead_ms' { return Get-OptionalMetric $Trial 'start.timing.boot_to_ready_ms' }
        'idle_cpu_median' { return Get-IdleMedian $Trial 'cpu_seconds_per_10s' }
        'idle_working_set_median' { return Get-IdleMedian $Trial 'working_set_bytes' }
        'idle_private_median' { return Get-IdleMedian $Trial 'private_bytes' }
        'install_debug_ms' { return Get-OptionalMetric $Trial 'install_debug.elapsed_ms' }
        'first_launch_this_time_ms' { return Get-OptionalMetric $Trial 'first_launch.this_time_ms' }
        'first_launch_total_time_ms' { return Get-OptionalMetric $Trial 'first_launch.total_time_ms' }
        'first_launch_wait_time_ms' { return Get-OptionalMetric $Trial 'first_launch.wait_time_ms' }
        'connected_test_wall_ms' { return Get-OptionalMetric $Trial 'connected_android_test.elapsed_ms' }
        'connected_test_suite_seconds' { return Get-OptionalMetric $Trial 'connected_android_test.reported_suite_seconds' }
        default { throw "Unknown metric: $Metric" }
    }
}

function Convert-TrialToCsv($Trial, [string]$RunStatus = 'unknown') {
    $required = @{ pair = 'Integer'; condition = 'String'; order = 'Integer'; success = 'Boolean' }
    foreach ($name in $required.Keys) {
        $property = $Trial.PSObject.Properties[$name]
        $valid = $null -ne $property -and $null -ne $property.Value
        if ($valid -and $required[$name] -eq 'Integer') { $valid = $property.Value -is [int] -or $property.Value -is [long] }
        elseif ($valid) { $valid = $property.Value.GetType().Name -eq $required[$name] }
        if (!$valid) { throw "Unexpected trial schema: '$name' missing or invalid" }
    }
    $expectedFields = @('pair','condition','order','trial','timestamp_utc','serial','reset_ok','start','idle','install_debug','first_launch','connected_android_test','stop_ok','success','notes','failures','host_load','process','devices')
    $actualFields = @($Trial.PSObject.Properties | ForEach-Object Name | Sort-Object)
    if (($actualFields -join ',') -ne (@($expectedFields | Sort-Object) -join ',')) { throw 'Unexpected trial schema: trial property set differs from canonical schema' }
    $containers = @{
        idle = @('stabilization_seconds','samples')
        first_launch = @('host_elapsed_ms','status','activity','this_time_ms','total_time_ms','wait_time_ms','complete','raw_output')
    }
    foreach ($name in $containers.Keys) {
        $container = $Trial.PSObject.Properties[$name].Value
        if ($null -eq $container) { throw "Unexpected trial schema: '$name' container is null" }
        $actual = @($container.PSObject.Properties | ForEach-Object Name | Sort-Object)
        if (($actual -join ',') -ne (@($containers[$name] | Sort-Object) -join ',')) { throw "Unexpected trial schema: '$name' property set differs from canonical schema" }
    }
    foreach ($name in @('install_debug','connected_android_test')) {
        $container = $Trial.PSObject.Properties[$name].Value
        if ($null -eq $container) { throw "Unexpected trial schema: '$name' container is null" }
        $common = if ($name -eq 'install_debug') { @('elapsed_ms','exit_code','outcome','task_outcomes','unexpected_recompile') } else { @('elapsed_ms','exit_code','outcome','tests','failures','errors','skipped','reported_suite_seconds','reports') }
        $actual = @($container.PSObject.Properties | ForEach-Object Name | Sort-Object) -join ','
        $variants = @(@($common + @('stdout','stderr')),@($common + 'output_excerpt')) | ForEach-Object { @($_ | Sort-Object) -join ',' }
        if ($actual -notin $variants) { throw "Unexpected trial schema: '$name' property set differs from canonical phase schema" }
    }
    if ($null -ne $Trial.start) {
        $startFields = @('exit_code','timed_out','command_wall_ms','serial','expected_serial','timing','boot_complete_ms','stdout','stderr')
        if ((@($Trial.start.PSObject.Properties | ForEach-Object Name | Sort-Object) -join ',') -ne (@($startFields | Sort-Object) -join ',')) { throw 'Unexpected trial schema: start property set differs from canonical schema' }
    }
    $suiteSeconds = Get-OptionalMetric $Trial 'connected_android_test.reported_suite_seconds'
    return [pscustomobject]@{
        pair = $Trial.pair
        condition = $Trial.condition
        order = $Trial.order
        boot_complete_ms = Get-MetricValue $Trial 'boot_complete_ms'
        start_total_ms = Get-MetricValue $Trial 'start_total_ms'
        idle_cpu_median = Get-MetricValue $Trial 'idle_cpu_median'
        idle_working_set_median = Get-MetricValue $Trial 'idle_working_set_median'
        idle_private_median = Get-MetricValue $Trial 'idle_private_median'
        install_debug_ms = Get-MetricValue $Trial 'install_debug_ms'
        first_launch_this_time_ms = Get-MetricValue $Trial 'first_launch_this_time_ms'
        first_launch_total_time_ms = Get-MetricValue $Trial 'first_launch_total_time_ms'
        first_launch_wait_time_ms = Get-MetricValue $Trial 'first_launch_wait_time_ms'
        connected_test_wall_ms = Get-MetricValue $Trial 'connected_test_wall_ms'
        connected_test_suite_ms = if ($null -ne $suiteSeconds) { [math]::Round($suiteSeconds * 1000) } else { $null }
        success = [bool]$Trial.success
        notes = ($Trial.notes -join '; ')
        run_status = $RunStatus
    }
}

function Convert-AvdRamToMb([string]$ConfigText) {
    $ramMatch = [regex]::Match($ConfigText, '(?m)^hw\.ramSize=(?<value>[^\r\n]*)$')
    if (!$ramMatch.Success) { throw 'managed AVD RAM missing from config.ini' }
    $ramText = $ramMatch.Groups['value'].Value.Trim()
    $valueMatch = [regex]::Match($ramText, '^(?<value>\d+)(?<unit>[KkMmGg]?)$')
    if (!$valueMatch.Success) { throw "unrecognized configured RAM: $ramText" }
    $amount = [long]0
    if (![long]::TryParse($valueMatch.Groups['value'].Value, [Globalization.NumberStyles]::None, [Globalization.CultureInfo]::InvariantCulture, [ref]$amount)) {
        throw "configured RAM value is out of range: $ramText"
    }
    $unit = $valueMatch.Groups['unit'].Value.ToUpperInvariant()
    switch ($unit) {
        'G' {
            if ($amount -gt 8) { throw "configured RAM exceeds 8192 MB: $ramText" }
            $ramMb = $amount * 1024L
        }
        'M' {
            if ($amount -gt 8192) { throw "configured RAM exceeds 8192 MB: $ramText" }
            $ramMb = $amount
        }
        'K' {
            if ($amount -gt 8388608) { throw "configured RAM exceeds 8192 MB: $ramText" }
            $ramMb = [long][math]::Round($amount / 1024.0)
        }
        default {
            if ($amount -gt 8192) { throw "configured RAM exceeds 8192 MB: $ramText" }
            $ramMb = $amount
        }
    }
    if ($ramMb -lt 1 -or $ramMb -gt 8192) { throw "configured RAM is outside 1–8192 MB: $ramText" }
    return [pscustomobject]@{ configured_ram = $ramText; ram_mb = [long]$ramMb }
}

function Get-XmlSuites([xml]$Xml) {
    $root = $Xml.DocumentElement
    if ($null -eq $root) { throw 'connected test XML has no document element' }
    switch ($root.LocalName) {
        'testsuite' { return $root }
        'testsuites' {
            $suites = @($root.SelectNodes('./testsuite'))
            if ($suites.Count -eq 0) { throw 'connected test XML contains no test suites' }
            return $suites
        }
        default { throw "unexpected connected test XML root: $($root.LocalName)" }
    }
}

function Require-DryRunField($Object, [string]$Path, [string]$Code) {
    try { $value = Get-OptionalMetric $Object $Path } catch { throw "${Code}:$Path" }
    if ($null -eq $value -or ($value -is [string] -and [string]::IsNullOrWhiteSpace($value))) { throw "${Code}:$Path" }
    return $value
}

function Test-CompletedDryRun($Run, [string]$ExpectedHarnessHash, [ref]$Reason = $null, [bool]$RequireBinaryIdentity = $false) {
    if ($null -ne $Reason) { $Reason.Value = 'UNKNOWN' }
    try {
        if ($null -eq $Run -or (Require-DryRunField $Run 'status' 'MISSING_FIELD') -ne 'dry_run_complete') { throw 'RUN_NOT_COMPLETE' }
        if ((Require-DryRunField $Run 'schema_version' 'MISSING_FIELD') -ne 1 -or (Require-DryRunField $Run 'benchmark_version' 'MISSING_FIELD') -ne 'windows-product-proof-1') { throw 'SCHEMA_MISMATCH' }
        if ((Require-DryRunField $Run 'harness_sha256' 'IDENTITY_MISSING') -ne $ExpectedHarnessHash) { throw 'HARNESS_IDENTITY_MISMATCH' }
        foreach ($path in @('emutrim_commit','emutrim_version','fixture_repository','fixture_commit','host.windows_version','host.windows_build','runtime.managed_home','runtime.avd_name','runtime.avd_config_sha256','runtime.configured_ram_mb','runtime.emulator_version','runtime.emulator_package_revision','runtime.system_image','runtime.api_level','runtime.abi')) {
            $null = Require-DryRunField $Run $path 'IDENTITY_MISSING'
        }
        if ($Run.runtime.avd_name -ne $script:AvdName -or $Run.runtime.system_image -notlike "*android-$($Run.runtime.api_level)*;$($Run.runtime.abi)" -or $Run.host.windows_version -notlike "*.$($Run.host.windows_build)") { throw 'RUN_IDENTITY_MISMATCH' }
        if ($Run.runtime.emulator_version -notmatch '\(build_id \d+\)' -or $Run.runtime.emulator_version -notlike "*$($Run.runtime.emulator_package_revision)*") { throw 'EMULATOR_IDENTITY_MISMATCH' }
        $binaryHash = $null
        if ($Run.runtime -is [System.Collections.IDictionary]) {
            if ($Run.runtime.Contains('emulator_binary_sha256')) { $binaryHash = $Run.runtime['emulator_binary_sha256'] }
        } else {
            $binaryProperty = $Run.runtime.PSObject.Properties['emulator_binary_sha256']
            if ($null -ne $binaryProperty) { $binaryHash = $binaryProperty.Value }
        }
        if ($RequireBinaryIdentity -and [string]::IsNullOrWhiteSpace($binaryHash)) { throw 'EMULATOR_BINARY_IDENTITY_MISSING' }
        if ($binaryHash -and $binaryHash -notmatch '^[A-Fa-f0-9]{64}$') { throw 'EMULATOR_BINARY_IDENTITY_MISMATCH' }
        $imageProperties = @(Require-DryRunField $Run 'runtime.system_image_properties' 'IDENTITY_MISSING')
        if (!($imageProperties | Where-Object { $_ -match '^Pkg.Revision=\d+$' }) -or !($imageProperties | Where-Object { $_ -eq "AndroidVersion.ApiLevel=$($Run.runtime.api_level)" }) -or !($imageProperties | Where-Object { $_ -match '^AndroidVersion.ExtensionLevel=\d+$' })) { throw 'IMAGE_IDENTITY_MISMATCH' }
        foreach ($path in @('protocol.control','protocol.treatment','protocol.reset','protocol.pair_order','protocol.stabilization_seconds','protocol.idle_sample_count','protocol.idle_sample_seconds','protocol.cold_boot','protocol.ram_unchanged')) {
            $null = Require-DryRunField $Run $path 'PROTOCOL_MISSING'
        }
        if ($Run.protocol.stabilization_seconds -ne 120 -or $Run.protocol.idle_sample_count -ne 5 -or $Run.protocol.idle_sample_seconds -ne 10 -or $Run.protocol.cold_boot -ne $true -or $Run.protocol.ram_unchanged -ne $true) { throw 'PROTOCOL_MISMATCH' }
        if ($Run.protocol.control -notlike '*--no-slim*' -or $Run.protocol.treatment -like '*--no-slim*' -or $Run.protocol.reset -notlike '*reset EmuTrim_Managed --yes*') { throw 'PROTOCOL_MISMATCH' }
        $order = @(Get-ConditionOrder)
        $pairOrder = @($Run.protocol.pair_order)
        if ($pairOrder.Count -ne $order.Count) { throw 'PAIR_ORDER_MISMATCH' }
        for ($i = 0; $i -lt $order.Count; $i++) {
            if ($pairOrder[$i].pair -ne $order[$i].pair -or $pairOrder[$i].condition -ne $order[$i].condition -or $pairOrder[$i].order -ne $order[$i].order) { throw 'PAIR_ORDER_MISMATCH' }
        }
        if (@($Run.failures).Count -ne 0) { throw 'RUN_HAS_FAILURES' }
        $warmups = @($Run.warmups)
        $trials = @($Run.trials)
        if ($warmups.Count -ne 2 -or $trials.Count -ne 2) { throw 'PHASE_COUNT_MISMATCH' }
        for ($i = 0; $i -lt 2; $i++) {
            $expected = $order[$i]
            $warm = $warmups[$i]
            foreach ($field in @('condition','trial','reset_ok','start_ok','health_ok','stop_ok','failure_phase','failure')) {
                if ($null -eq $warm.PSObject.Properties[$field]) { throw "PHASE_MISSING:warmups[$i].$field" }
            }
            if ($warm.condition -ne $expected.condition -or $warm.trial -ne 'warmup') { throw 'WARMUP_ORDER_MISMATCH' }
            if ($warm.reset_ok -ne $true -or $warm.start_ok -ne $true -or $warm.health_ok -ne $true -or $warm.stop_ok -ne $true -or $warm.failure_phase -or $warm.failure) { throw 'WARMUP_INCOMPLETE' }
            $trial = $trials[$i]
            foreach ($field in @('pair','condition','order','trial','success','reset_ok','stop_ok','failures')) {
                if ($null -eq $trial.PSObject.Properties[$field]) { throw "PHASE_MISSING:trials[$i].$field" }
            }
            if ($trial.pair -ne $expected.pair -or $trial.condition -ne $expected.condition -or $trial.order -ne $expected.order -or $trial.trial -ne ($i + 1)) { throw 'TRIAL_ORDER_MISMATCH' }
            if ($trial.success -ne $true -or $trial.reset_ok -ne $true -or $trial.stop_ok -ne $true -or @($trial.failures).Count -ne 0) { throw 'TRIAL_INCOMPLETE' }
            $serial = Require-DryRunField $trial 'serial' 'PHASE_MISSING'
            if ($serial -notmatch '^emulator-\d+$') { throw 'TARGET_IDENTITY_MISMATCH' }
            foreach ($path in @('start.exit_code','start.timed_out','start.serial','start.boot_complete_ms','idle.stabilization_seconds','idle.samples','install_debug.exit_code','install_debug.outcome','install_debug.elapsed_ms','install_debug.task_outcomes','first_launch.status','first_launch.complete','first_launch.total_time_ms','first_launch.wait_time_ms','connected_android_test.exit_code','connected_android_test.outcome','connected_android_test.tests','connected_android_test.failures','connected_android_test.errors','connected_android_test.skipped','connected_android_test.reports')) {
                $null = Require-DryRunField $trial $path 'PHASE_MISSING'
            }
            if ($trial.start.serial -ne $serial -or $trial.start.exit_code -ne 0 -or $trial.start.timed_out -ne $false -or $trial.start.boot_complete_ms -le 0) { throw 'START_INCOMPLETE_OR_IDENTITY_MISMATCH' }
            $trialBinaryHash = $null
            if ($trial.start -is [System.Collections.IDictionary]) {
                if ($trial.start.Contains('emulator_binary_sha256')) { $trialBinaryHash = $trial.start['emulator_binary_sha256'] }
            } else {
                $trialBinaryProperty = $trial.start.PSObject.Properties['emulator_binary_sha256']
                if ($null -ne $trialBinaryProperty) { $trialBinaryHash = $trialBinaryProperty.Value }
            }
            if ($RequireBinaryIdentity -and [string]::IsNullOrWhiteSpace($trialBinaryHash)) { throw 'EMULATOR_BINARY_IDENTITY_MISSING' }
            if ($binaryHash -or $trialBinaryHash) {
                if (!$binaryHash -or !$trialBinaryHash -or $trialBinaryHash -notmatch '^[A-Fa-f0-9]{64}$' -or $trialBinaryHash -ne $binaryHash) { throw 'EMULATOR_BINARY_IDENTITY_MISMATCH' }
            }
            if ($trial.idle.stabilization_seconds -ne 120) { throw 'STABILIZATION_MISMATCH' }
            $samples = @($trial.idle.samples)
            if ($samples.Count -ne 5) { throw 'SAMPLES_INCOMPLETE' }
            for ($j = 0; $j -lt 5; $j++) {
                if ($samples[$j].index -ne ($j + 1) -or $samples[$j].interval_seconds -lt 10 -or $null -eq $samples[$j].cpu_seconds_per_10s -or $null -eq $samples[$j].working_set_bytes -or $null -eq $samples[$j].private_bytes) { throw 'SAMPLES_INVALID' }
            }
            if ($trial.install_debug.exit_code -ne 0 -or $trial.install_debug.outcome -ne 'SUCCESS' -or $trial.install_debug.elapsed_ms -lt 0 -or $trial.first_launch.status -ne 'ok' -or $trial.first_launch.complete -ne $true -or $trial.first_launch.total_time_ms -lt 0 -or $trial.first_launch.wait_time_ms -lt 0) { throw 'FIXTURE_PHASE_INCOMPLETE' }
            if ($trial.connected_android_test.exit_code -ne 0 -or $trial.connected_android_test.outcome -ne 'SUCCESS' -or $trial.connected_android_test.tests -ne 4 -or $trial.connected_android_test.failures -ne 0 -or $trial.connected_android_test.errors -ne 0 -or $trial.connected_android_test.skipped -ne 0) { throw 'TEST_PHASE_INCOMPLETE' }
        }
        if ($null -ne $Reason) { $Reason.Value = 'VALID' }
        return $true
    } catch {
        if ($null -ne $Reason) { $Reason.Value = $_.Exception.Message }
        return $false
    }
}

function Get-EmulatorBinaryHash {
    $path = Join-Path $script:ManagedSdk 'emulator\emulator.exe'
    try {
        $file = Get-Item -LiteralPath $path -ErrorAction Stop
        if (!$file.PSIsContainer -and $file.Extension -eq '.exe') {
            return (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256 -ErrorAction Stop).Hash
        }
    } catch { throw "EMULATOR_BINARY_IDENTITY_UNAVAILABLE: $($_.Exception.Message)" }
    throw "EMULATOR_BINARY_IDENTITY_UNAVAILABLE: executable missing: $path"
}

function Test-Parsers {
    [xml]$singleSuiteXml = '<testsuite name="single" tests="4" failures="0" errors="0" skipped="0" time="1.25" />'
    $singleSuites = @(Get-XmlSuites $singleSuiteXml)
    if ($singleSuites.Count -ne 1 -or $singleSuites[0].tests -ne '4') { throw 'single testsuite XML self-check failed' }
    [xml]$groupedSuiteXml = '<testsuites><testsuite name="first" tests="2" failures="0" errors="0" skipped="0" time="0.5" /><testsuite name="second" tests="2" failures="0" errors="0" skipped="0" time="0.75" /></testsuites>'
    $groupedSuites = @(Get-XmlSuites $groupedSuiteXml)
    if ($groupedSuites.Count -ne 2 -or $groupedSuites[1].name -ne 'second') { throw 'testsuites XML self-check failed' }
    foreach ($case in @(
        @('hw.ramSize=4096', 4096L),
        @('hw.ramSize=4096M', 4096L),
        @('hw.ramSize=4G', 4096L),
        @('hw.ramSize=2G', 2048L),
        @('hw.ramSize=4194304K', 4096L),
        @('hw.ramSize=4g', 4096L),
        @('hw.ramSize=4096m', 4096L)
    )) {
        $parsedRam = Convert-AvdRamToMb $case[0]
        if ($parsedRam.ram_mb -ne $case[1]) { throw "RAM parser self-check failed for $($case[0])" }
    }
    foreach ($invalidRam in @('hw.ramSize=', 'hw.ramSize=banana', 'hw.ramSize=4T', 'hw.ramSize=-1', 'hw.ramSize=999999999999999999999G', 'missing ram entry')) {
        try { $null = Convert-AvdRamToMb $invalidRam; throw "RAM parser accepted invalid input: $invalidRam" } catch {
            if ($_.Exception.Message -like 'RAM parser accepted invalid input:*') { throw }
        }
    }
    $metadataRam = Convert-AvdRamToMb "target=android-37.0`nhw.ramSize=4096`n"
    if ($metadataRam.ram_mb -ne 4096) { throw 'runtime RAM metadata self-check failed' }
    $timing = Parse-StartupTiming 'startup timing: launch→console 1.2s; console→ADB 2.3s; ADB→device 0.1s; device→boot 4.0s; boot→ready 0.4s; total 8.0s'
    if ($timing.launch_to_console_ms -ne 1200 -or $timing.device_to_boot_ms -ne 4000 -or $timing.total_ms -ne 8000 -or $timing.boot_complete_ms_from_phases -ne 7600) { throw 'timing parser self-check failed' }
    $unavailable = Parse-StartupTiming 'startup timing: launch→console 1.0s; console→ADB overlap; ADB→device 0.0s; device→boot 2.0s; boot→ready n/a; total 3.2s'
    if ($null -ne $unavailable.console_to_adb_ms -or $null -ne $unavailable.boot_complete_ms_from_phases) { throw 'timing missing-phase self-check failed' }
    $am = Parse-ActivityLaunch "Status: ok`nActivity: $script:MainActivity`nThisTime: 123`nTotalTime: 125`nWaitTime: 300`nComplete`n" 456
    if ($am.status -ne 'ok' -or $am.activity -ne $script:MainActivity -or $am.this_time_ms -ne 123 -or $am.total_time_ms -ne 125 -or $am.wait_time_ms -ne 300 -or !$am.complete -or $am.host_elapsed_ms -ne 456) { throw 'am start -W parser self-check failed' }
    $math = Get-Statistics ([double[]]@(1, 2, 3, 4, 5))
    if ($math.median -ne 3 -or $math.mean -ne 3 -or $math.min -ne 1 -or $math.max -ne 5 -or $math.n -ne 5) { throw 'statistics self-check failed' }
    $order = (Get-ConditionOrder | ForEach-Object { "$($_.pair):$($_.condition)" }) -join ','
    if ($order -ne '1:CONTROL,1:SLIM,2:SLIM,2:CONTROL,3:CONTROL,3:SLIM,4:SLIM,4:CONTROL,5:CONTROL,5:SLIM') { throw 'condition-order self-check failed' }
    $record = [pscustomobject]@{ condition = 'SLIM'; pair = 2; trial = 4; failures = @() }
    Add-Failure $record 'install_debug' 'fixture failure' 1 1250
    if ($record.failures.Count -ne 1 -or $record.failures[0].pair -ne 2 -or $record.failures[0].elapsed_ms -ne 1250) { throw 'failure-retention self-check failed' }
    $sample = [pscustomobject]@{ index=1; interval_seconds=10.01; cpu_seconds_per_10s=0.1; working_set_bytes=100; private_bytes=200 }
    $trial = [pscustomobject]@{
        pair=1; condition='CONTROL'; order=1; trial=1; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; success=$true; failures=@()
        start=[pscustomobject]@{ exit_code=0; timed_out=$false; serial='emulator-5554'; boot_complete_ms=1000 }
        idle=[pscustomobject]@{ stabilization_seconds=120; samples=@(1..5 | ForEach-Object { $copy=$sample | Select-Object *; $copy.index=$_; $copy }) }
        install_debug=[pscustomobject]@{ exit_code=0; outcome='SUCCESS'; elapsed_ms=100; task_outcomes=@([pscustomobject]@{ name=':app:installDebug'; outcome='EXECUTED' }) }
        first_launch=[pscustomobject]@{ status='ok'; complete=$true; total_time_ms=100; wait_time_ms=100 }
        connected_android_test=[pscustomobject]@{ exit_code=0; outcome='SUCCESS'; tests=4; failures=0; errors=0; skipped=0; reports=@('synthetic.xml') }
    }
    $slim = $trial | ConvertTo-Json -Depth 20 | ConvertFrom-Json
    $slim.condition='SLIM'; $slim.order=2; $slim.trial=2
    $syntheticHash = 'A' * 64
    $trial.start | Add-Member -NotePropertyName emulator_binary_sha256 -NotePropertyValue $syntheticHash
    $slim.start | Add-Member -NotePropertyName emulator_binary_sha256 -NotePropertyValue $syntheticHash
    $eligibleDryRun = [pscustomobject]@{
        schema_version=1; benchmark_version='windows-product-proof-1'; status='dry_run_complete'; harness_sha256='candidate'
        emutrim_commit='test-commit'; emutrim_version='0.7.0'; fixture_repository='https://github.com/android/testing-samples'; fixture_commit='test-fixture'
        host=[pscustomobject]@{ windows_version='10.0.26200'; windows_build='26200' }
        runtime=[pscustomobject]@{ managed_home='test-home'; avd_name='EmuTrim_Managed'; avd_config_sha256='test-config'; configured_ram_mb=4096; emulator_version='Android emulator version 37.2.11.0 (build_id 16416033)'; emulator_binary_sha256=$syntheticHash; emulator_package_revision='37.2.11'; system_image='system-images;android-37.0;google_apis;x86_64'; system_image_properties=@('Pkg.Revision=6','AndroidVersion.ApiLevel=37.0','AndroidVersion.ExtensionLevel=22'); api_level='37.0'; abi='x86_64' }
        protocol=[pscustomobject]@{ control='EmuTrim start EmuTrim_Managed --no-slim'; treatment='EmuTrim start EmuTrim_Managed'; reset='EmuTrim reset EmuTrim_Managed --yes'; pair_order=@(Get-ConditionOrder); stabilization_seconds=120; idle_sample_count=5; idle_sample_seconds=10; cold_boot=$true; ram_unchanged=$true }
        warmups=@([pscustomobject]@{ condition='CONTROL'; trial='warmup'; reset_ok=$true; start_ok=$true; health_ok=$true; stop_ok=$true; failure_phase=$null; failure=$null },[pscustomobject]@{ condition='SLIM'; trial='warmup'; reset_ok=$true; start_ok=$true; health_ok=$true; stop_ok=$true; failure_phase=$null; failure=$null })
        trials=@($trial,$slim); failures=@()
    }
    $reason = ''
    if (!(Test-CompletedDryRun $eligibleDryRun 'candidate' ([ref]$reason)) -or $reason -ne 'VALID') { throw "valid paired dry-run self-check failed: $reason" }
    if (!(Test-CompletedDryRun $eligibleDryRun 'candidate' ([ref]$reason) $true) -or $reason -ne 'VALID') { throw "binary identity official self-check failed: $reason" }
    $rejectCases = @(
        @{ name='missing receipt'; mutate={ param($v) $v.status=$null }; reason='MISSING_FIELD:status' },
        @{ name='wrong trial order'; mutate={ param($v) $v.trials[0].condition='SLIM' }; reason='TRIAL_ORDER_MISMATCH' },
        @{ name='wrong protocol order'; mutate={ param($v) $v.protocol.pair_order[0].condition='SLIM' }; reason='PAIR_ORDER_MISMATCH' },
        @{ name='duplicate warmup'; mutate={ param($v) $v.warmups[1].condition='CONTROL' }; reason='WARMUP_ORDER_MISMATCH' },
        @{ name='missing phase'; mutate={ param($v) $v.trials[0].install_debug=$null }; reason='PHASE_MISSING:install_debug.exit_code' },
        @{ name='aborted phase'; mutate={ param($v) $v.trials[0].stop_ok=$false }; reason='TRIAL_INCOMPLETE' },
        @{ name='missing samples'; mutate={ param($v) $v.trials[0].idle.samples=@() }; reason='PHASE_MISSING:idle.samples' },
        @{ name='insufficient samples'; mutate={ param($v) $v.trials[0].idle.samples=@($v.trials[0].idle.samples | Select-Object -First 4) }; reason='SAMPLES_INCOMPLETE' },
        @{ name='short sample'; mutate={ param($v) $v.trials[0].idle.samples[0].interval_seconds=1 }; reason='SAMPLES_INVALID' },
        @{ name='identity mismatch'; mutate={ param($v) $v.trials[0].start.serial='emulator-5556' }; reason='START_INCOMPLETE_OR_IDENTITY_MISMATCH' },
        @{ name='run identity mismatch'; mutate={ param($v) $v.runtime.avd_name='OtherAVD' }; reason='RUN_IDENTITY_MISMATCH' },
        @{ name='missing identity'; mutate={ param($v) $v.runtime.PSObject.Properties.Remove('emulator_version') }; reason='IDENTITY_MISSING:runtime.emulator_version' },
        @{ name='missing host identity'; mutate={ param($v) $v.host.windows_build=$null }; reason='IDENTITY_MISSING:host.windows_build' },
        @{ name='harness mismatch'; mutate={ param($v) $v.harness_sha256='old-candidate' }; reason='HARNESS_IDENTITY_MISMATCH' },
        @{ name='incomplete tests'; mutate={ param($v) $v.trials[0].connected_android_test.tests=3 }; reason='TEST_PHASE_INCOMPLETE' }
    )
    foreach ($case in $rejectCases) {
        $copy = $eligibleDryRun | ConvertTo-Json -Depth 30 | ConvertFrom-Json
        & $case.mutate $copy
        $reason = ''
        if ((Test-CompletedDryRun $copy 'candidate' ([ref]$reason)) -or $reason -ne $case.reason) { throw "dry-run $($case.name) self-check failed: $reason" }
    }
    $missingBinary = $eligibleDryRun | ConvertTo-Json -Depth 30 | ConvertFrom-Json
    $missingBinary.runtime.PSObject.Properties.Remove('emulator_binary_sha256')
    $reason = ''
    if ((Test-CompletedDryRun $missingBinary 'candidate' ([ref]$reason) $true) -or $reason -ne 'EMULATOR_BINARY_IDENTITY_MISSING') { throw "missing binary identity self-check failed: $reason" }
    foreach ($case in @(
        @{ name='missing trial binary'; mutate={ param($v) $v.trials[0].start.PSObject.Properties.Remove('emulator_binary_sha256') } },
        @{ name='null runtime binary'; mutate={ param($v) $v.runtime.emulator_binary_sha256=$null } },
        @{ name='empty runtime binary'; mutate={ param($v) $v.runtime.emulator_binary_sha256='' } },
        @{ name='null trial binary'; mutate={ param($v) $v.trials[0].start.emulator_binary_sha256=$null } },
        @{ name='empty trial binary'; mutate={ param($v) $v.trials[0].start.emulator_binary_sha256='' } }
    )) {
        $copy = $eligibleDryRun | ConvertTo-Json -Depth 30 | ConvertFrom-Json
        & $case.mutate $copy
        $reason = ''
        if ((Test-CompletedDryRun $copy 'candidate' ([ref]$reason) $true) -or $reason -ne 'EMULATOR_BINARY_IDENTITY_MISSING') { throw "$($case.name) self-check failed: $reason" }
    }
    $historicalBinary = $missingBinary | ConvertTo-Json -Depth 30 | ConvertFrom-Json
    foreach ($item in $historicalBinary.trials) { $item.start.PSObject.Properties.Remove('emulator_binary_sha256') }
    if (!(Test-CompletedDryRun $historicalBinary 'candidate' ([ref]$reason)) -or $reason -ne 'VALID') { throw "historical binary structural self-check failed: $reason" }
    if ((Test-CompletedDryRun $historicalBinary 'candidate' ([ref]$reason) $true) -or $reason -ne 'EMULATOR_BINARY_IDENTITY_MISSING') { throw "historical binary official self-check failed: $reason" }
    $mismatchedBinary = $eligibleDryRun | ConvertTo-Json -Depth 30 | ConvertFrom-Json
    $mismatchedBinary.trials[1].start.emulator_binary_sha256 = 'B' * 64
    if ((Test-CompletedDryRun $mismatchedBinary 'candidate' ([ref]$reason) $true) -or $reason -ne 'EMULATOR_BINARY_IDENTITY_MISMATCH') { throw "paired binary mismatch self-check failed: $reason" }
    $savedSdk = $script:ManagedSdk
    try {
        $script:ManagedSdk = Join-Path ([System.IO.Path]::GetTempPath()) ('missing-emulator-' + [guid]::NewGuid().ToString('N'))
        try { $null = Get-EmulatorBinaryHash; throw 'missing executable accepted' } catch {
            if ($_.Exception.Message -notlike 'EMULATOR_BINARY_IDENTITY_UNAVAILABLE:*') { throw }
        }
    } finally { $script:ManagedSdk = $savedSdk }
    $lockedRoot = Join-Path ([System.IO.Path]::GetTempPath()) ('locked-emulator-' + [guid]::NewGuid().ToString('N'))
    $lockedPath = Join-Path $lockedRoot 'emulator\emulator.exe'
    $lock = $null
    try {
        $null = New-Item -ItemType Directory -Path (Split-Path $lockedPath -Parent) -Force
        [System.IO.File]::WriteAllBytes($lockedPath, [byte[]]@(1,2,3))
        $lock = [System.IO.File]::Open($lockedPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::None)
        $script:ManagedSdk = $lockedRoot
        try { $null = Get-EmulatorBinaryHash; throw 'unreadable executable accepted' } catch {
            if ($_.Exception.Message -notlike 'EMULATOR_BINARY_IDENTITY_UNAVAILABLE:*') { throw }
        }
    } finally {
        $script:ManagedSdk = $savedSdk
        if ($null -ne $lock) { $lock.Dispose() }
        if (Test-Path $lockedRoot) {
            $resolvedLockRoot = (Resolve-Path -LiteralPath $lockedRoot).Path
            if (!$resolvedLockRoot.StartsWith([System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()), [System.StringComparison]::OrdinalIgnoreCase)) { throw 'temporary executable path escaped temp root' }
            Remove-Item -LiteralPath $resolvedLockRoot -Recurse -Force
        }
    }
    if ($script:OfficialBaselineApproved) { throw 'provisional Beta became official-eligible' }
    $csv = Convert-TrialToCsv ([pscustomobject]@{
        pair = 1; condition = 'CONTROL'; order = 1; success = $true; notes = @(); trial=1; timestamp_utc='fixture'; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; failures=@(); host_load=$null; process=$null; devices=$null;
        start = [pscustomobject]@{ exit_code=0;timed_out=$false;boot_complete_ms = 1; command_wall_ms = 2;serial='emulator-5554';expected_serial='emulator-5554';timing = [pscustomobject]@{ boot_to_ready_ms = 0 };stdout='';stderr='' };
        idle = [pscustomobject]@{ stabilization_seconds=120; samples = @([pscustomobject]@{ cpu_seconds_per_10s = 0.5; working_set_bytes = 10; private_bytes = 8 }) };
        install_debug = [pscustomobject]@{ elapsed_ms = 3; exit_code=$null; outcome='SUCCESS'; task_outcomes=@(); unexpected_recompile=$false; output_excerpt=$null };
        first_launch = [pscustomobject]@{ host_elapsed_ms=$null; status='ok'; activity='fixture'; this_time_ms = 4; total_time_ms = 5; wait_time_ms = 6; complete=$true; raw_output='' };
        connected_android_test = [pscustomobject]@{ elapsed_ms = 7; exit_code=$null; outcome='SUCCESS'; tests=1; failures=0; errors=0; skipped=0; reported_suite_seconds = 0.2; reports=@(); output_excerpt=$null }
    })
    if ($csv.install_debug_ms -ne 3 -or $csv.connected_test_suite_ms -ne 200 -or $csv.idle_working_set_median -ne 10) { throw 'CSV flattening self-check failed' }
    $expectedColumns = @('pair','condition','order','boot_complete_ms','start_total_ms','idle_cpu_median','idle_working_set_median','idle_private_median','install_debug_ms','first_launch_this_time_ms','first_launch_total_time_ms','first_launch_wait_time_ms','connected_test_wall_ms','connected_test_suite_ms','success','notes','run_status')
    $completedControl = Convert-TrialToCsv ([pscustomobject]@{
        pair=1; condition='CONTROL'; order=1; success=$true; notes=@(); trial=1; timestamp_utc='fixture'; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; failures=@(); host_load=$null; process=$null; devices=$null;
        start=[pscustomobject]@{exit_code=0;timed_out=$false;boot_complete_ms=7600;command_wall_ms=8000;serial='emulator-5554';expected_serial='emulator-5554';timing=[pscustomobject]@{boot_to_ready_ms=400};stdout='';stderr=''};
        idle=[pscustomobject]@{stabilization_seconds=120;samples=@()}; install_debug=[pscustomobject]@{elapsed_ms=1;exit_code=0;outcome='SUCCESS';task_outcomes=@();unexpected_recompile=$false;output_excerpt=$null};
        first_launch=[pscustomobject]@{host_elapsed_ms=5;status='ok';activity='fixture';this_time_ms=2;total_time_ms=3;wait_time_ms=4;complete=$true;raw_output=''};
        connected_android_test=[pscustomobject]@{elapsed_ms=5;exit_code=0;outcome='SUCCESS';tests=1;failures=0;errors=0;skipped=0;reported_suite_seconds=0.006;reports=@();output_excerpt=$null}
    }) 'complete'
    $completedSlim = Convert-TrialToCsv ([pscustomobject]@{
        pair=1; condition='SLIM'; order=2; success=$true; notes=@(); trial=2; timestamp_utc='fixture'; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; failures=@(); host_load=$null; process=$null; devices=$null;
        start=[pscustomobject]@{exit_code=0;timed_out=$false;boot_complete_ms=7600;command_wall_ms=8000;serial='emulator-5554';expected_serial='emulator-5554';timing=[pscustomobject]@{boot_to_ready_ms=400};stdout='';stderr=''};
        idle=[pscustomobject]@{stabilization_seconds=120;samples=@()}; install_debug=[pscustomobject]@{elapsed_ms=1;exit_code=0;outcome='SUCCESS';task_outcomes=@();unexpected_recompile=$false;output_excerpt=$null};
        first_launch=[pscustomobject]@{host_elapsed_ms=5;status='ok';activity='fixture';this_time_ms=2;total_time_ms=3;wait_time_ms=4;complete=$true;raw_output=''};
        connected_android_test=[pscustomobject]@{elapsed_ms=5;exit_code=0;outcome='SUCCESS';tests=1;failures=0;errors=0;skipped=0;reported_suite_seconds=0.006;reports=@();output_excerpt=$null}
    }) 'complete'
    $completedCsv = @(@($completedControl, $completedSlim) | ConvertTo-Csv -NoTypeInformation)
    if (($completedCsv[0] -replace '"','').Split(',') -join ',' -ne ($expectedColumns -join ',') -or $completedCsv.Count -ne 3) { throw 'CSV completed CONTROL/SLIM schema self-check failed' }
    if ($completedCsv[1] -notmatch '"7600"' -or $completedCsv[2] -notmatch '"7600"') { throw 'CSV boot timing self-check failed' }
    $failedReset = [pscustomobject]@{
        pair=3; condition='CONTROL'; order=1; success=$false; notes=@(); trial=5; timestamp_utc='fixture'; serial=$null; reset_ok=$false; stop_ok=$false; failures=@(); host_load=$null; process=$null; devices=$null; start=$null;
        idle=[pscustomobject]@{stabilization_seconds=120;samples=@()}; install_debug=[pscustomobject]@{elapsed_ms=$null;exit_code=$null;outcome='NOT_RUN';task_outcomes=@();unexpected_recompile=$false;stdout='';stderr=''};
        first_launch=[pscustomobject]@{host_elapsed_ms=$null;status=$null;activity=$null;this_time_ms=$null;total_time_ms=$null;wait_time_ms=$null;complete=$false;raw_output=''};
        connected_android_test=[pscustomobject]@{elapsed_ms=$null;exit_code=$null;outcome='NOT_RUN';tests=$null;failures=$null;errors=$null;skipped=$null;reported_suite_seconds=$null;reports=@();stdout='';stderr=''}
    }
    $failedResetRow = Convert-TrialToCsv $failedReset 'aborted'
    $failedStart = $failedReset | Select-Object *
    $failedStart.start = [pscustomobject]@{exit_code=1;timed_out=$false;boot_complete_ms=$null;command_wall_ms=$null;serial=$null;expected_serial='';timing=$null;stdout='';stderr=''}
    $failedStartRow = Convert-TrialToCsv $failedStart 'aborted'
    if ($failedResetRow.success -or $failedStartRow.success -or $failedResetRow.run_status -ne 'aborted' -or $null -ne $failedResetRow.boot_complete_ms -or $null -ne $failedStartRow.start_total_ms) { throw 'CSV failed pre-measurement trial self-check failed' }
    $failedCsv = @(@($failedResetRow,$failedStartRow) | ConvertTo-Csv -NoTypeInformation)
    if ($failedCsv.Count -ne 3 -or $failedCsv[1] -notmatch 'aborted' -or $failedCsv[2] -notmatch 'aborted') { throw 'CSV failed observation serialization self-check failed' }
    $missingMetric = [pscustomobject]@{
        pair=1; condition='CONTROL'; order=1; success=$true; notes=@(); trial=1; timestamp_utc='fixture'; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; failures=@(); host_load=$null; process=$null; devices=$null;
        start=[pscustomobject]@{exit_code=0;timed_out=$false;boot_complete_ms=7600;command_wall_ms=8000;serial='emulator-5554';expected_serial='emulator-5554';timing=[pscustomobject]@{boot_to_ready_ms=400};stdout='';stderr=''};
        idle=[pscustomobject]@{stabilization_seconds=120;samples=@()}; install_debug=[pscustomobject]@{elapsed_ms=$null;exit_code=$null;outcome='NOT_RUN';task_outcomes=@();unexpected_recompile=$false;output_excerpt=$null};
        first_launch=[pscustomobject]@{host_elapsed_ms=5;status='ok';activity='fixture';this_time_ms=2;total_time_ms=3;wait_time_ms=4;complete=$true;raw_output=''};
        connected_android_test=[pscustomobject]@{elapsed_ms=5;exit_code=0;outcome='SUCCESS';tests=1;failures=0;errors=0;skipped=0;reported_suite_seconds=$null;reports=@();output_excerpt=$null}
    }
    $missingMetric.install_debug.elapsed_ms = $null
    if ($null -ne (Convert-TrialToCsv $missingMetric).install_debug_ms) { throw 'CSV unavailable metric self-check failed' }
    $badSchema = [pscustomobject]@{
        pair=1; condition='CONTROL'; order=1; success=$true; notes=@(); trial=1; timestamp_utc='fixture'; serial='emulator-5554'; reset_ok=$true; stop_ok=$true; failures=@(); host_load=$null; process=$null; devices=$null; rogue=$true;
        start=[pscustomobject]@{exit_code=0;timed_out=$false;boot_complete_ms=7600;command_wall_ms=8000;serial='emulator-5554';expected_serial='emulator-5554';timing=[pscustomobject]@{boot_to_ready_ms=400};stdout='';stderr=''};
        idle=[pscustomobject]@{stabilization_seconds=120;samples=@()}; install_debug=[pscustomobject]@{elapsed_ms=1;exit_code=0;outcome='SUCCESS';task_outcomes=@();unexpected_recompile=$false;output_excerpt=$null};
        first_launch=[pscustomobject]@{host_elapsed_ms=5;status='ok';activity='fixture';this_time_ms=2;total_time_ms=3;wait_time_ms=4;complete=$true;raw_output=''};
        connected_android_test=[pscustomobject]@{elapsed_ms=5;exit_code=0;outcome='SUCCESS';tests=1;failures=0;errors=0;skipped=0;reported_suite_seconds=0.006;reports=@();output_excerpt=$null}
    }
    try { $null = Convert-TrialToCsv $badSchema; throw 'CSV accepted unexpected schema' } catch { if ($_.Exception.Message -eq 'CSV accepted unexpected schema') { throw } }
    $badRequired = $completedControl | Select-Object *
    $badRequired.PSObject.Properties.Remove('pair')
    try { $null = Convert-TrialToCsv $badRequired; throw 'CSV accepted missing required field' } catch { if ($_.Exception.Message -eq 'CSV accepted missing required field') { throw } }
    'runtime RAM metadata self-check: ram_mb=4096'
    'parser self-checks: PASS (RAM units/invalids/overflow, testsuite XML roots, timings, am start -W, statistics, CSV, order, failure retention, dry-run eligibility; 15 legacy rejection cases; binary SHA cases)'
}

if ($Mode -eq 'SelfTest') {
    Test-Parsers
    if (![string]::IsNullOrWhiteSpace($InputRawPath)) {
        $historical = Get-Content -LiteralPath $InputRawPath -Raw | ConvertFrom-Json
        $reason = ''
        if (!(Test-CompletedDryRun $historical $historical.harness_sha256 ([ref]$reason))) { throw "historical dry-run structural self-check failed: $reason" }
        'historical dry-run structural self-check: PASS'
    }
    exit 0
}
if ($Mode -eq 'Official' -and !$script:OfficialBaselineApproved) { throw 'OFFICIAL_BASELINE_UNSELECTED: no v0.8 Emulator baseline is approved for official measurements' }

function Invoke-Captured {
    param(
        [string]$FilePath,
        [string[]]$Arguments,
        [string]$WorkingDirectory,
        [hashtable]$Environment,
        [int]$TimeoutSeconds = 180
    )
    $psi = [System.Diagnostics.ProcessStartInfo]::new()
    $psi.FileName = $FilePath
    $psi.WorkingDirectory = $WorkingDirectory
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    foreach ($argument in $Arguments) { [void]$psi.ArgumentList.Add($argument) }
    foreach ($key in $Environment.Keys) { $psi.Environment[$key] = [string]$Environment[$key] }
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    if (!$process.Start()) { throw "Could not start $FilePath" }
    $capture = [ProcessPipeCapture]::new($process)
    $timedOut = !$process.WaitForExit($TimeoutSeconds * 1000)
    if ($timedOut) {
        try { $process.Kill($true) } catch { }
        [void]$process.WaitForExit(5000)
    }
    $capture.Stop()
    $watch.Stop()
    return [pscustomobject]@{
        exit_code = if ($timedOut) { $null } else { $process.ExitCode }
        timed_out = $timedOut
        elapsed_ms = [long]$watch.ElapsedMilliseconds
        stdout = $capture.Stdout
        stderr = $capture.Stderr
        started_utc = [datetime]::UtcNow.AddMilliseconds(-$watch.ElapsedMilliseconds).ToString('o')
    }
}

function Invoke-EmuTrim([string[]]$Arguments, [int]$TimeoutSeconds = 180) {
    return Invoke-Captured $script:EmuTrim $Arguments $script:Repo @{ EMUTRIM_HOME = $script:RuntimeRoot } $TimeoutSeconds
}

function Invoke-ADB([string[]]$Arguments, [int]$TimeoutSeconds = 45) {
    $adb = Join-Path $script:ManagedSdk 'platform-tools\adb.exe'
    return Invoke-Captured $adb $Arguments $script:Repo @{} $TimeoutSeconds
}

function Invoke-Gradle([string]$TaskMode, [string]$Serial = '', [int]$TimeoutSeconds = 900) {
    $helper = Join-Path $script:BenchmarkRoot 'Invoke-FixtureGradle.ps1'
    if (!(Test-Path $helper)) { throw "fixture Gradle launcher missing: $helper" }
    $envMap = @{
        ANDROID_HOME = $script:BuildSdk
        ANDROID_SDK_ROOT = $script:BuildSdk
        JAVA_HOME = $script:JavaHome
    }
    if ($Serial) { $envMap.ANDROID_SERIAL = $Serial }
    return Invoke-Captured $script:PowerShell @('-NoProfile', '-File', $helper, $TaskMode) $script:Repo $envMap $TimeoutSeconds
}

function Get-AdbDevices {
    $result = Invoke-ADB @('devices', '-l')
    if ($result.exit_code -ne 0) { throw "adb devices failed: $($result.stderr)" }
    $devices = @()
    foreach ($line in ($result.stdout -split "`r?`n")) {
        if ($line -match '^\s*(?<serial>\S+)\s+(?<state>device|offline|unauthorized|no permissions)\b(?<details>.*)$') {
            $devices += [pscustomobject]@{ serial = $Matches.serial; state = $Matches.state; details = $Matches.details.Trim() }
        }
    }
    return [pscustomobject]@{ raw = $result.stdout.Trim(); devices = $devices }
}

function Get-ListenerOwners([int]$Port) {
    $rows = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue)
    return @($rows | Select-Object -ExpandProperty OwningProcess -Unique)
}

function Get-ManagedEmulatorProcesses {
    $root = (Join-Path $script:ManagedSdk 'emulator').TrimEnd('\') + '\'
    return @(Get-CimInstance Win32_Process | Where-Object { $_.Name -eq 'emulator.exe' -and $_.ExecutablePath -and $_.ExecutablePath.StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase) } | ForEach-Object { [pscustomobject]@{ pid = $_.ProcessId; command_line = $_.CommandLine } })
}

function Get-HostSnapshot {
    $os = Get-CimInstance Win32_OperatingSystem
    $computer = Get-CimInstance Win32_ComputerSystem
    $processors = @(Get-CimInstance Win32_Processor)
    $gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object { [pscustomobject]@{ name = $_.Name; driver_version = $_.DriverVersion; driver_date = $_.DriverDate } })
    $battery = @(Get-CimInstance -Namespace root\wmi -ClassName BatteryStatus -ErrorAction SilentlyContinue)
    $power = if ($battery.Count -eq 0) { 'AC or no battery reported' } elseif ($battery[0].PowerOnline) { 'AC' } else { 'battery' }
    return [pscustomobject]@{
        windows_caption = $os.Caption
        windows_version = $os.Version
        windows_build = $os.BuildNumber
        cpu = @($processors | ForEach-Object { [pscustomobject]@{ name = $_.Name.Trim(); cores = $_.NumberOfCores; logical_processors = $_.NumberOfLogicalProcessors } })
        logical_cores = ($processors | Measure-Object -Property NumberOfLogicalProcessors -Sum).Sum
        physical_ram_bytes = [long]$computer.TotalPhysicalMemory
        gpus = $gpus
        power_source = $power
    }
}

function Get-HostLoad {
    $cpu = $null
    try { $cpu = (Get-Counter '\Processor(_Total)\% Processor Time' -SampleInterval 1 -MaxSamples 1).CounterSamples[0].CookedValue } catch { }
    $os = Get-CimInstance Win32_OperatingSystem
    return [pscustomobject]@{
        timestamp_utc = [datetime]::UtcNow.ToString('o')
        total_cpu_percent = if ($null -ne $cpu) { [math]::Round([double]$cpu, 2) } else { $null }
        available_physical_memory_bytes = [long]$os.FreePhysicalMemory * 1024
    }
}

function Get-ConsoleProcess([string]$Serial) {
    if ($Serial -notmatch '^emulator-(?<port>\d+)$') { throw "invalid emulator serial: $Serial" }
    $port = [int]$Matches.port
    $probe = Invoke-EmuTrim @('stats', $Serial, '--seconds=1') 15
    $expected = '(?m)^AVD:\s*EmuTrim_Managed\s+serial:\s*' + [regex]::Escape($Serial) + '\s+process:'
    if ($probe.exit_code -ne 0 -or $probe.stdout -notmatch $expected) {
        throw "EmuTrim stats did not authenticate expected console ${Serial}: $($probe.stderr) $($probe.stdout)"
    }
    if ($probe.stdout -notmatch '(?m)^PID:\s*(\d+)\s*$') { throw 'EmuTrim stats omitted console PID' }
    $pidValue = [int]$Matches[1]
    $owners = @(Get-ListenerOwners $port)
    if ($owners.Count -ne 1 -or [int]$owners[0] -ne $pidValue) { throw "console port $port owner mismatch: stats=$pidValue listeners=$($owners -join ',')" }
    $process = Get-CimInstance Win32_Process -Filter "ProcessId=$pidValue"
    $emulatorDir = (Join-Path $script:ManagedSdk 'emulator').TrimEnd('\') + '\'
    if ($null -eq $process -or !$process.ExecutablePath.StartsWith($emulatorDir, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "console PID $pidValue executable is outside managed Emulator package"
    }
    return [pscustomobject]@{
        pid = $pidValue
        port = $port
        executable = $process.ExecutablePath
        process_name = $process.Name
        command_line = $process.CommandLine
        start_time_utc = (Get-Process -Id $pidValue).StartTime.ToUniversalTime().ToString('o')
        avd = $script:AvdName
        serial = $Serial
    }
}

function Get-ProcessSample([int]$PidValue) {
    $p = Get-Process -Id $PidValue -ErrorAction Stop
    return [pscustomobject]@{
        pid = $PidValue
        cpu_seconds = [double]$p.CPU
        working_set_bytes = [long]$p.WorkingSet64
        private_bytes = [long]$p.PrivateMemorySize64
        thread_count = $p.Threads.Count
        handle_count = $p.HandleCount
        sample_utc = [datetime]::UtcNow.ToString('o')
    }
}

function Get-XmlSuiteSummary([datetime]$SinceUtc, [hashtable]$BeforeHashes) {
    $dir = Join-Path $script:FixtureProject 'app\build\outputs\androidTest-results\connected\debug'
    $files = @(Get-ChildItem $dir -Filter 'TEST-*.xml' -File -Recurse -ErrorAction SilentlyContinue)
    $fresh = @()
    foreach ($file in $files) {
        $hash = (Get-FileHash $file.FullName -Algorithm SHA256).Hash
        if ($file.LastWriteTimeUtc -ge $SinceUtc -or $BeforeHashes[$file.FullName] -ne $hash) { $fresh += $file }
    }
    $tests = 0; $failures = 0; $errors = 0; $skipped = 0; $seconds = 0.0
    foreach ($file in $fresh) {
        [xml]$xml = Get-Content -LiteralPath $file.FullName -Raw
        $suites = Get-XmlSuites $xml
        foreach ($suite in $suites) {
            $tests += [int]$suite.tests
            $failures += [int]$suite.failures
            $errors += [int]$suite.errors
            $skipped += [int]$suite.skipped
            $seconds += [double]$suite.time
        }
    }
    if ($fresh.Count -eq 0) { return $null }
    return [pscustomobject]@{
        files = @($fresh | ForEach-Object FullName)
        tests = $tests
        failures = $failures
        errors = $errors
        skipped = $skipped
        reported_suite_seconds = $seconds
    }
}

function Get-ResultObject([object[]]$Trials, [object[]]$Warmups, [object[]]$Failures, [string]$Status) {
    $commit = (& git -C $script:Repo rev-parse HEAD).Trim()
    $branch = (& git -C $script:Repo rev-parse --abbrev-ref HEAD).Trim()
    return [ordered]@{
        schema_version = 1
        benchmark_version = 'windows-product-proof-1'
        status = $Status
        generated_utc = [datetime]::UtcNow.ToString('o')
        emutrim_commit = $commit
        emutrim_branch = $branch
        harness_sha256 = (Get-FileHash -LiteralPath $script:HarnessPath -Algorithm SHA256).Hash
        emutrim_version = '0.7.0'
        fixture_repository = 'https://github.com/android/testing-samples'
        fixture_commit = '8c9df3a534ef99e44d481d96c00a5fc1970f7c70'
        fixture = [ordered]@{ project = 'ui/espresso/BasicSample'; application_id = $script:ApplicationId; main_activity = $script:MainActivity; compile_sdk = 34; instrumentation_runner = 'androidx.test.runner.AndroidJUnitRunner'; gradle = '8.7'; agp = '8.5.0'; kotlin = '1.9.22'; license = 'Apache-2.0' }
        host = $null
        runtime = $null
        protocol = [ordered]@{ control = 'EmuTrim start EmuTrim_Managed --managed --headless --cold-boot --no-slim --timings'; treatment = 'EmuTrim start EmuTrim_Managed --managed --headless --cold-boot --timings'; reset = 'EmuTrim reset EmuTrim_Managed --yes'; pair_order = @(Get-ConditionOrder); stabilization_seconds = 120; idle_sample_count = 5; idle_sample_seconds = 10; cold_boot = $true; ram_unchanged = $true }
        warmups = $Warmups
        trials = $Trials
        failures = $Failures
    }
}

function Write-JsonFile($Value, [string]$Path) {
    $json = ConvertTo-Json -InputObject $Value -Depth 40
    [System.IO.File]::WriteAllText($Path, $json + "`n", [System.Text.UTF8Encoding]::new($false))
}

function Assert-FixtureAndSdk {
    if (!(Test-Path $script:BenchmarkRoot) -or !(Test-Path $script:Fixture)) { throw 'benchmark workspace or pinned fixture missing' }
    $lock = Get-Content -LiteralPath $script:FixtureLockPath -Raw | ConvertFrom-Json
    if ((& git -C $script:Fixture rev-parse HEAD).Trim() -ne $lock.commit) { throw 'fixture HEAD differs from fixture.lock.json' }
    $fixtureDiff = @(& git -C $script:Fixture diff -- ui/espresso/BasicSample/build.gradle)
    if ((@(& git -C $script:Fixture status --porcelain) -join '').Trim() -ne 'M ui/espresso/BasicSample/build.gradle' -or
        (@(& git -C $script:Fixture diff --name-only) -join "`n") -ne 'ui/espresso/BasicSample/build.gradle' -or
        (@($fixtureDiff | Select-String '^\+\s+espressoVersion = "3\.7\.0"$').Count -ne 1) -or
        (@($fixtureDiff | Select-String '^\-\s+espressoVersion = "3\.6\.1"$').Count -ne 1) -or
        ((& git -C $script:Fixture diff --numstat).Trim() -ne "1`t1`tui/espresso/BasicSample/build.gradle")) { throw 'fixture differs from the authorized one-line Espresso compatibility patch' }
    if (Test-Path (Join-Path $script:FixtureProject 'local.properties')) { throw 'fixture local.properties exists; refusing to modify/use it' }
    if (!(Test-Path (Join-Path $script:BuildSdk 'platforms\android-34\android.jar')) -or !(Test-Path (Join-Path $script:BuildSdk 'build-tools\34.0.0\source.properties'))) { throw 'dedicated fixture build SDK packages missing' }
    $onlineLog = Join-Path $script:BenchmarkRoot 'fixture-prepare.stdout.txt'
    $offlineLog = Join-Path $script:BenchmarkRoot 'fixture-offline-prepare.stdout.txt'
    if (!(Test-Path $onlineLog) -or !(Select-String -Path $onlineLog -Pattern 'BUILD SUCCESSFUL' -Quiet)) { throw 'untimed online fixture preparation did not pass' }
    if (!(Test-Path $offlineLog) -or !(Select-String -Path $offlineLog -Pattern 'BUILD SUCCESSFUL' -Quiet)) { throw 'offline fixture assembly preflight did not pass' }
    if (!(Test-Path (Join-Path $script:ManagedSdk 'emulator\emulator.exe')) -or !(Test-Path (Join-Path $script:ManagedSdk 'platform-tools\adb.exe'))) { throw 'dedicated EmuTrim managed runtime incomplete' }
    if ((& git -C $script:Repo rev-parse --abbrev-ref HEAD).Trim() -ne 'master') { throw 'expected canonical branch master' }
    if ((& git -C $script:Repo rev-parse HEAD).Trim() -ne 'e6a6da2868ce47502e4b5a58588d1a4bff5ec89a') { throw 'EmuTrim source changed from benchmark commit' }
    if ((& git -C $script:Repo rev-parse origin/master).Trim() -ne 'e6a6da2868ce47502e4b5a58588d1a4bff5ec89a') { throw 'origin/master changed from benchmark commit' }
    if ((& $script:EmuTrim --version).Trim() -ne 'emutrim 0.7.0') { throw 'EmuTrim binary version is not 0.7.0' }
    if (!(Test-Path $script:PowerShell)) { throw 'PowerShell 7 executable missing for Gradle process isolation' }
}

function Start-AdbServer($Lifecycle) {
    $owners = @(Get-ListenerOwners 5037)
    $Lifecycle.existed_before = ($owners.Count -gt 0)
    if ($owners.Count -eq 0) {
        $started = Invoke-ADB @('start-server')
        if ($started.exit_code -ne 0) { throw "managed adb start-server failed: $($started.stderr)" }
        $owners = @(Get-ListenerOwners 5037)
        if ($owners.Count -ne 1) { throw "expected one ADB server listener after start-server; found $($owners.Count)" }
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($owners[0])"
        $expected = (Join-Path $script:ManagedSdk 'platform-tools\adb.exe')
        if ($null -eq $proc -or !$proc.ExecutablePath.Equals($expected, [System.StringComparison]::OrdinalIgnoreCase)) { throw 'ADB listener is not the managed SDK adb server' }
        $Lifecycle.started_by_harness = $true
        $Lifecycle.pid = [int]$owners[0]
        $Lifecycle.executable = $proc.ExecutablePath
        $Lifecycle.start_server = $started.stdout.Trim()
        $script:AdbServerPid = [int]$owners[0]
        $script:AdbServerExecutable = $proc.ExecutablePath
        $script:AdbServerCurrentExecutable = $proc.ExecutablePath
        $Lifecycle.current_executable = $proc.ExecutablePath
    } else {
        if ($owners.Count -ne 1) { throw "ambiguous ADB server listeners on port 5037: $($owners -join ',')" }
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($owners[0])"
        $Lifecycle.pid = [int]$owners[0]
        $Lifecycle.executable = if ($proc) { $proc.ExecutablePath } else { $null }
        $Lifecycle.started_by_harness = $false
        $script:AdbServerPid = [int]$owners[0]
        $script:AdbServerExecutable = $Lifecycle.executable
        $script:AdbServerCurrentExecutable = $Lifecycle.executable
        $Lifecycle.current_executable = $Lifecycle.executable
    }
    $devices = Get-AdbDevices
    $Lifecycle.devices_before_trials = $devices
    if (@($devices.devices | Where-Object { $_.state -eq 'device' -and $_.serial -notmatch '^emulator-\d+$' }).Count -gt 0) {
        throw 'physical ADB device is online; refusing benchmark'
    }
}

function Assert-AdbServerStable($Lifecycle, [string]$Phase) {
    $owners = @(Get-ListenerOwners 5037)
    if ($owners.Count -ne 1) { throw "ADB server listener count changed: found $($owners.Count)" }
    $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$($owners[0])"
    $allowed = @((Join-Path $script:ManagedSdk 'platform-tools\adb.exe'), (Join-Path $script:BuildSdk 'platform-tools\adb.exe'))
    if ($null -eq $proc -or $proc.ExecutablePath -notin $allowed) { throw "ADB server executable is outside benchmark SDKs: $($proc.ExecutablePath)" }
    $version = (& $proc.ExecutablePath version 2>&1 | Out-String).Trim()
    if ($version -notmatch 'Android Debug Bridge version 1\.0\.41' -or $version -notmatch 'Version 37\.0\.1') { throw "unexpected benchmark platform-tools server version: $version" }
    $snapshot = [pscustomobject]@{ pid = [int]$owners[0]; executable = $proc.ExecutablePath; version = $version; timestamp_utc = [datetime]::UtcNow.ToString('o') }
    if ($script:AdbServerCurrentExecutable -and !$script:AdbServerCurrentExecutable.Equals($proc.ExecutablePath, [System.StringComparison]::OrdinalIgnoreCase)) {
        $Lifecycle.transitions += [pscustomobject]@{ from = $script:AdbServerCurrentExecutable; to = $proc.ExecutablePath; at_utc = $snapshot.timestamp_utc; after_phase = $Phase }
    }
    $script:AdbServerCurrentExecutable = $proc.ExecutablePath
    $script:AdbServerPid = [int]$owners[0]
    $Lifecycle.current_executable = $proc.ExecutablePath
    $Lifecycle.last_snapshot = $snapshot
    return $snapshot
}

function Assert-TargetOnly([string]$Serial) {
    $devices = Get-AdbDevices
    if (@($devices.devices | Where-Object { $_.state -eq 'device' -and $_.serial -eq $Serial }).Count -ne 1) { throw "exact benchmark target $Serial is not online" }
    $others = @($devices.devices | Where-Object { $_.state -eq 'device' -and $_.serial -ne $Serial })
    if ($others.Count -gt 0) { throw "other online ADB target(s) present; refusing Gradle workload: $($others.serial -join ',')" }
    return $devices
}

function Get-ProcessInventorySnapshot([string]$Root) {
    $inventory = [ordered]@{ path = $Root; platforms = @(); build_tools = @(); system_images = @() }
    foreach ($entry in @(@('platforms', 'platforms'), @('build_tools', 'build-tools'), @('system_images', 'system-images'))) {
        $path = Join-Path $Root $entry[1]
        $inventory[$entry[0]] = @(Get-ChildItem -LiteralPath $path -Directory -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Name | Sort-Object)
    }
    return [pscustomobject]$inventory
}

function Get-RuntimeMetadata {
    $manifest = Get-Content (Join-Path $script:ManagedRoot 'manifest.json') -Raw | ConvertFrom-Json
    if ($manifest.avds.Count -ne 1 -or $manifest.avds[0] -ne $script:AvdName) { throw 'managed manifest does not contain exactly the benchmark AVD' }
    $config = Join-Path $script:ManagedRoot 'avd\EmuTrim_Managed.avd\config.ini'
    $configText = Get-Content $config -Raw
    $ram = Convert-AvdRamToMb $configText
    $ramText = $ram.configured_ram
    $ramMb = $ram.ram_mb
    $imagePackage = [string]$manifest.system_image
    $imagePath = Join-Path $script:ManagedSdk ('system-images\' + $imagePackage.Replace(';', '\').Substring('system-images\'.Length))
    $imageProps = Get-Content (Join-Path $imagePath 'source.properties')
    $emulatorProps = Get-Content (Join-Path $script:ManagedSdk 'emulator\source.properties') -Raw
    $emulatorVersion = (& (Join-Path $script:ManagedSdk 'emulator\emulator.exe') -version 2>&1 | Select-Object -First 1).ToString().Trim()
    $adbVersion = (& (Join-Path $script:ManagedSdk 'platform-tools\adb.exe') version 2>&1 | Out-String).Trim()
    $accel = (& (Join-Path $script:ManagedSdk 'emulator\emulator.exe') -accel-check 2>&1 | Out-String).Trim()
    return [pscustomobject]@{
        managed_home = $script:RuntimeRoot
        runtime_managed_sdk = $script:ManagedSdk
        avd_name = $script:AvdName
        avd_config_sha256 = (Get-FileHash $config -Algorithm SHA256).Hash
        configured_ram = $ramText
        configured_ram_mb = $ramMb
        avd_config = [ordered]@{ entries = @($configText -split "`r?`n" | Where-Object { $_ -match '^[^#=]+=.*$' }); abi_type = ([regex]::Match($configText, '(?m)^abi\.type=(.+)$').Groups[1].Value); cpu_cores = ([regex]::Match($configText, '(?m)^hw\.cpu\.ncore=(\d+)').Groups[1].Value); gpu_enabled = ([regex]::Match($configText, '(?m)^hw\.gpu\.enabled=(.+)$').Groups[1].Value); gpu_mode = ([regex]::Match($configText, '(?m)^hw\.gpu\.mode=(.+)$').Groups[1].Value); audio_input = ([regex]::Match($configText, '(?m)^hw\.audioInput=(.+)$').Groups[1].Value); audio_output = ([regex]::Match($configText, '(?m)^hw\.audioOutput=(.+)$').Groups[1].Value); camera_back = ([regex]::Match($configText, '(?m)^hw\.camera\.back=(.+)$').Groups[1].Value); camera_front = ([regex]::Match($configText, '(?m)^hw\.camera\.front=(.+)$').Groups[1].Value) }
        emulator_version = $emulatorVersion
        emulator_binary_sha256 = Get-EmulatorBinaryHash
        emulator_package_revision = ([regex]::Match($emulatorProps, '(?m)^Pkg.Revision=(.+)$').Groups[1].Value)
        system_image = $imagePackage
        system_image_properties = @($imageProps | Where-Object { $_ -match '^(Pkg.Revision|AndroidVersion.ApiLevel|AndroidVersion.ExtensionLevel)=' })
        api_level = (($imageProps | Where-Object { $_ -match '^AndroidVersion.ApiLevel=' }) -replace '^.*=', '')
        abi = 'x86_64'
        platform_tools_revision = ((Get-Content (Join-Path $script:ManagedSdk 'platform-tools\source.properties') | Where-Object { $_ -match '^Pkg.Revision=' }) -replace '^.*=', '')
        adb_version = $adbVersion
        whpx = $accel
        page_size = $null
    }
}

function Get-BuildSdkMetadata {
    $androidCliVersion = (& $script:AndroidCli --version 2>&1 | Out-String).Trim()
    $packages = @()
    foreach ($package in @('platforms\android-34', 'build-tools\34.0.0', 'platform-tools')) {
        $path = Join-Path $script:BuildSdk $package
        if (Test-Path $path) {
            $source = Join-Path $path 'source.properties'
            $revision = if (Test-Path $source) { ((Get-Content $source | Where-Object { $_ -match '^Pkg.Revision=' }) -replace '^.*=', '') -join '' } else { $null }
            $packageName = switch ($package) {
                'platforms\android-34' { 'platforms;android-34' }
                'build-tools\34.0.0' { 'build-tools;34.0.0' }
                'platform-tools' { 'platform-tools' }
            }
            $packages += [pscustomobject]@{ package = $packageName; revision = $revision; path = $path }
        }
    }
    return [pscustomobject]@{
        build_sdk_root = $script:BuildSdk
        android_cli = $script:AndroidCli
        android_cli_version = $androidCliVersion
        packages = $packages
        jdk = [pscustomobject]@{ java_home = $script:JavaHome; java_executable = (Join-Path $script:JavaHome 'bin\java.exe'); java_version = (& (Join-Path $script:JavaHome 'bin\java.exe') -version 2>&1 | Out-String).Trim() }
        gradle_version_output = (Get-Content (Join-Path $script:BenchmarkRoot 'gradle-version.stdout.txt') -Raw).Trim()
        gradle_online_prepare_exit_code = 0
        gradle_offline_prepare_exit_code = 0
        gradle_preparation = [pscustomobject]@{
            online_log = Join-Path $script:BenchmarkRoot 'fixture-prepare.stdout.txt'
            offline_log = Join-Path $script:BenchmarkRoot 'fixture-offline-prepare.stdout.txt'
            apk_paths = @(Get-ChildItem (Join-Path $script:FixtureProject 'app\build\outputs') -Filter '*.apk' -File -Recurse | ForEach-Object { [pscustomobject]@{ path=$_.FullName; sha256=(Get-FileHash $_.FullName -Algorithm SHA256).Hash; bytes=$_.Length } })
        }
        android_cli_installations = @(
            [pscustomobject]@{ package='platforms/android-34'; command='android.exe --sdk=<build-sdk> sdk install platforms/android-34'; exit_code=0; license_prompted=$false; stdout_log=(Join-Path $env:TEMP 'emutrim-benchmark-080-install-platform.stdout.txt'); stderr_log=(Join-Path $env:TEMP 'emutrim-benchmark-080-install-platform.stderr.txt') },
            [pscustomobject]@{ package='build-tools/34.0.0'; command='android.exe --sdk=<build-sdk> sdk install build-tools/34.0.0'; exit_code=0; license_prompted=$false; stdout_log=(Join-Path $env:TEMP 'emutrim-benchmark-080-install-build-tools.stdout.txt'); stderr_log=(Join-Path $env:TEMP 'emutrim-benchmark-080-install-build-tools.stderr.txt') }
        )
        external_android_studio_sdk = $script:ExternalSdkRoot
        external_sdk_modified = $false
    }
}

function Get-FixtureMetadata {
    $lock = Get-Content $script:FixtureLockPath -Raw | ConvertFrom-Json
    $rootBuild = Get-Content (Join-Path $script:FixtureProject 'build.gradle') -Raw
    $appBuild = Get-Content (Join-Path $script:FixtureProject 'app\build.gradle') -Raw
    $manifest = Get-Content (Join-Path $script:FixtureProject 'app\src\main\AndroidManifest.xml') -Raw
    if ($rootBuild -notmatch 'agpVersion\s*=\s*"8\.5\.0"' -or $rootBuild -notmatch 'kotlinVersion\s*=\s*"1\.9\.22"') { throw 'fixture AGP/Kotlin values differ from lock' }
    if ($appBuild -notmatch 'compileSdk\s+34' -or $appBuild -notmatch [regex]::Escape($script:ApplicationId) -or $appBuild -notmatch 'androidx\.test\.runner\.AndroidJUnitRunner') { throw 'fixture build identity differs from lock' }
    if ($manifest -notmatch [regex]::Escape($script:MainActivity)) { throw 'fixture main activity differs from lock' }
    return [pscustomobject]@{
        repository = $lock.repository
        commit = $lock.commit
        project = $lock.project
        license = $lock.license
        application_id = $script:ApplicationId
        main_activity = $script:MainActivity
        compile_sdk = 34
        build_tools = '34.0.0'
        agp = '8.5.0'
        gradle = '8.7'
        kotlin = '1.9.22'
        instrumentation_runner = 'androidx.test.runner.AndroidJUnitRunner'
        checkout_clean = $false
        fixture_compatibility_patch = [ordered]@{
            espresso = [ordered]@{ from = '3.6.1'; to = '3.7.0' }
            reason = 'Android 37 compatibility; upstream Espresso fix replaces reflective InputManager.getInstance'
            patch_sha256 = (Get-FileHash (Join-Path $script:BenchmarkRoot 'fixture-compat.patch') -Algorithm SHA256).Hash
        }
        description = "Android testing-samples Espresso BasicSample; upstream commit $($lock.commit) with Espresso 3.7.0 Android 37 compatibility patch"
        resolved_android_test_runtime = [ordered]@{
            configuration = 'debugAndroidTestRuntimeClasspath'
            espresso_core = '3.7.0'
            test_core = '1.7.0'
            test_runner = '1.7.0'
            ext_junit = '1.2.1'
        }
        local_properties_present = $false
    }
}

function Get-XmlHashes {
    $dir = Join-Path $script:FixtureProject 'app\build\outputs\androidTest-results\connected\debug'
    $map = @{}
    foreach ($file in @(Get-ChildItem $dir -Filter 'TEST-*.xml' -File -Recurse -ErrorAction SilentlyContinue)) { $map[$file.FullName] = (Get-FileHash $file.FullName -Algorithm SHA256).Hash }
    return $map
}

function Get-StartTrial([string]$Condition, [string]$SerialExpected = '') {
    $binaryHash = Get-EmulatorBinaryHash
    if ($binaryHash -ne $script:EmulatorBinaryHash) { throw 'EMULATOR_BINARY_IDENTITY_MISMATCH' }
    $arguments = @('start', $script:AvdName, '--managed', '--headless', '--cold-boot')
    if ($Condition -eq 'CONTROL') { $arguments += '--no-slim' }
    $arguments += '--timings'
    $result = Invoke-EmuTrim $arguments 180
    $serial = $null
    if ($result.stdout -match '(?m)^started EmuTrim_Managed as (emulator-\d+)\s*$') { $serial = $Matches[1] }
    $timing = Parse-StartupTiming ($result.stdout + "`n" + $result.stderr)
    $bootMs = $null
    if ($null -ne $timing -and $null -ne $timing.boot_complete_ms_from_phases) { $bootMs = $timing.boot_complete_ms_from_phases }
    return [pscustomobject]@{
        exit_code = $result.exit_code
        timed_out = $result.timed_out
        command_wall_ms = $result.elapsed_ms
        serial = $serial
        expected_serial = $SerialExpected
        timing = $timing
        boot_complete_ms = $bootMs
        emulator_binary_sha256 = $binaryHash
        stdout = $result.stdout.Trim()
        stderr = $result.stderr.Trim()
    }
}

function Stop-Target([string]$Serial) {
    $target = if ($Serial) { $Serial } else { $script:AvdName }
    $result = Invoke-EmuTrim @('stop', $target) 30
    $closed = $true
    if ($Serial -match '^emulator-(\d+)$') { $closed = @(Get-ListenerOwners ([int]$Matches[1])).Count -eq 0 }
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $processes = @(Get-ManagedEmulatorProcesses)
    while ($processes.Count -gt 0 -and $watch.Elapsed.TotalSeconds -lt 45) {
        Start-Sleep -Milliseconds 500
        $processes = @(Get-ManagedEmulatorProcesses)
    }
    $watch.Stop()
    return [pscustomobject]@{ exit_code = $result.exit_code; timed_out = $result.timed_out; success = ($result.exit_code -eq 0 -and !$result.timed_out -and $closed -and $processes.Count -eq 0); console_closed = $closed; emulator_exit_wait_ms = $watch.ElapsedMilliseconds; remaining_processes = $processes; stdout = $result.stdout.Trim(); stderr = $result.stderr.Trim() }
}

function Reset-Target {
    $result = Invoke-EmuTrim @('reset', $script:AvdName, '--yes') 180
    return [pscustomobject]@{ exit_code = $result.exit_code; timed_out = $result.timed_out; success = ($result.exit_code -eq 0 -and !$result.timed_out); elapsed_ms = $result.elapsed_ms; stdout = $result.stdout.Trim(); stderr = $result.stderr.Trim() }
}

function Test-GuestHealth([string]$Serial) {
    $devices = Assert-TargetOnly $Serial
    $boot = Invoke-ADB @('-s', $Serial, 'shell', 'getprop', 'sys.boot_completed')
    $identity = Invoke-ADB @('-s', $Serial, 'shell', 'getprop', 'ro.kernel.qemu')
    return [pscustomobject]@{
        devices = $devices
        boot_completed = $boot.stdout.Trim()
        qemu_property = $identity.stdout.Trim()
        success = ($boot.exit_code -eq 0 -and $identity.exit_code -eq 0 -and $boot.stdout.Trim() -eq '1' -and $identity.stdout.Trim() -eq '1')
    }
}

function Invoke-Warmup([string]$Condition) {
    $reset = Reset-Target
    if (!$reset.success) { return [pscustomobject]@{ condition = $Condition; trial = 'warmup'; serial = $null; page_size = $null; reset_ok = $false; start_ok = $false; health_ok = $false; stop_ok = $false; failure_phase = 'reset'; exit_code = $reset.exit_code; elapsed_ms = $reset.elapsed_ms; failure = Get-Excerpt ($reset.stderr + "`n" + $reset.stdout) } }
    $start = Get-StartTrial $Condition
    if ($start.exit_code -ne 0 -or !$start.serial) {
        $stop = Stop-Target $start.serial
        return [pscustomobject]@{ condition = $Condition; trial = 'warmup'; serial = $start.serial; page_size = $null; reset_ok = $true; start_ok = $false; health_ok = $false; stop_ok = $stop.success; failure_phase = 'start'; exit_code = $start.exit_code; elapsed_ms = $start.command_wall_ms; failure = Get-Excerpt ($start.stderr + "`n" + $start.stdout); stop = $stop }
    }
    $health = Test-GuestHealth $start.serial
    $pageSize = Get-PageSize $start.serial
    $stop = Stop-Target $start.serial
    $phase = if (!$health.success) { 'health' } elseif (!$stop.success) { 'stop' } else { $null }
    $failure = if (!$health.success) { 'guest health verification failed' } elseif (!$stop.success) { Get-Excerpt ($stop.stderr + "`n" + $stop.stdout) } else { $null }
    return [pscustomobject]@{ condition = $Condition; trial = 'warmup'; serial = $start.serial; page_size = $pageSize; reset_ok = $true; start_ok = ($start.exit_code -eq 0); health_ok = $health.success; stop_ok = $stop.success; failure_phase = $phase; exit_code = if ($phase -eq 'stop') { $stop.exit_code } else { $null }; elapsed_ms = if ($phase -eq 'stop') { $stop.emulator_exit_wait_ms } else { $null }; failure = $failure; stop = $stop }
}

function Invoke-IdleSamples([int]$PidValue, [int]$Count, [int]$IntervalSeconds) {
    $samples = @()
    for ($index = 1; $index -le $Count; $index++) {
        $before = Get-ProcessSample $PidValue
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        Start-Sleep -Seconds $IntervalSeconds
        $after = Get-ProcessSample $PidValue
        $watch.Stop()
        $delta = [double]($after.cpu_seconds - $before.cpu_seconds)
        $samples += [pscustomobject]@{
            index = $index
            interval_seconds = [math]::Round($watch.Elapsed.TotalSeconds, 3)
            cpu_seconds = [math]::Round($delta, 6)
            cpu_seconds_per_10s = [math]::Round($delta * 10 / $watch.Elapsed.TotalSeconds, 6)
            working_set_bytes = $after.working_set_bytes
            private_bytes = $after.private_bytes
            thread_count = $after.thread_count
            handle_count = $after.handle_count
            sample_utc = $after.sample_utc
        }
    }
    return $samples
}

function Invoke-Trial($Order, [int]$TrialNumber, [int]$IdleSeconds, [int]$IdleCount, [string]$RunWorkDir, $Lifecycle) {
    $trial = [pscustomobject]@{
        pair = $Order.pair; condition = $Order.condition; order = $Order.order; trial = $TrialNumber
        timestamp_utc = [datetime]::UtcNow.ToString('o'); serial = $null; reset_ok = $false
        start = $null; idle = [pscustomobject]@{ stabilization_seconds = $IdleSeconds; samples = @() }
        install_debug = [pscustomobject]@{ elapsed_ms = $null; exit_code = $null; outcome = 'NOT_RUN'; task_outcomes = @(); unexpected_recompile = $false; stdout = ''; stderr = '' }
        first_launch = [pscustomobject]@{ host_elapsed_ms = $null; status = $null; activity = $null; this_time_ms = $null; total_time_ms = $null; wait_time_ms = $null; complete = $false; raw_output = '' }
        connected_android_test = [pscustomobject]@{ elapsed_ms = $null; exit_code = $null; outcome = 'NOT_RUN'; tests = $null; failures = $null; errors = $null; skipped = $null; reported_suite_seconds = $null; reports = @(); stdout = ''; stderr = '' }
        stop_ok = $false; success = $false; notes = @(); failures = @(); host_load = $null; process = $null; devices = $null
    }
    $reset = Reset-Target
    $trial.reset_ok = $reset.success
    if (!$reset.success) {
        Add-Failure $trial 'reset' ($reset.stderr + $reset.stdout) $reset.exit_code $reset.elapsed_ms
        return [pscustomobject]@{ trial = $trial; abort = $true }
    }
    $trial.host_load = Get-HostLoad
    $start = Get-StartTrial $Order.condition
    $trial.start = $start
    $trial.serial = $start.serial
    if ($start.exit_code -ne 0 -or !$start.serial -or $null -eq $start.timing) {
        Add-Failure $trial 'start' ($start.stderr + $start.stdout) $start.exit_code $start.command_wall_ms
        $stop = Stop-Target $start.serial
        $trial.stop_ok = $stop.success
        if (!$stop.success) { Add-Failure $trial 'stop' ($stop.stderr + $stop.stdout) $stop.exit_code $null; return [pscustomobject]@{ trial = $trial; abort = $true } }
        return [pscustomobject]@{ trial = $trial; abort = $false }
    }
    $phase = 'post_start'
    try {
        $phase = 'guest_health'
        $health = Test-GuestHealth $start.serial
        if (!$health.success) { throw 'post-start guest health or exact target verification failed' }
        $trial.devices = $health.devices
        $phase = 'process_identification'
        $console = Get-ConsoleProcess $start.serial
        $trial.process = $console
        $phase = 'idle_stabilization'
        Start-Sleep -Seconds $IdleSeconds
        $live = Get-Process -Id $console.pid -ErrorAction Stop
        if ($live.StartTime.ToUniversalTime().ToString('o') -ne $console.start_time_utc) { throw 'console owner process changed during idle stabilization' }
        $phase = 'idle_sampling'
        $trial.idle.samples = Invoke-IdleSamples $console.pid $IdleCount 10
        $phase = 'install_debug'
        $install = Invoke-Gradle 'install-debug' $start.serial 600
        [void](Assert-AdbServerStable $Lifecycle 'installDebug')
        $null = Assert-TargetOnly $start.serial
        $tasks = @([regex]::Matches($install.stdout, '(?m)^> Task (?<name>:\S+?)(?: (?<outcome>UP-TO-DATE|NO-SOURCE|FROM-CACHE|SKIPPED))?\s*$') | ForEach-Object { [pscustomobject]@{ name = $_.Groups['name'].Value; outcome = if ($_.Groups['outcome'].Success) { $_.Groups['outcome'].Value } else { 'EXECUTED' } } })
        $rebuild = @($tasks | Where-Object { $_.name -match '^:app:(compile|package|merge|dex|process.*Resources)' -and $_.outcome -notin @('UP-TO-DATE', 'NO-SOURCE', 'FROM-CACHE') }).Count -gt 0
        $trial.install_debug = [pscustomobject]@{ elapsed_ms = $install.elapsed_ms; exit_code = $install.exit_code; outcome = if ($install.exit_code -eq 0 -and !$install.timed_out) { 'SUCCESS' } else { 'FAILED' }; task_outcomes = $tasks; unexpected_recompile = $rebuild; output_excerpt = if ($install.exit_code -ne 0) { Get-Excerpt ($install.stderr + "`n" + $install.stdout) } else { $null } }
        if ($trial.install_debug.outcome -ne 'SUCCESS') { Add-Failure $trial 'install_debug' ($install.stderr + $install.stdout) $install.exit_code $install.elapsed_ms }
        if ($trial.install_debug.outcome -eq 'SUCCESS') {
            $phase = 'first_launch'
            $launchCommand = "am start -S -W -n $script:ApplicationId/$script:MainActivity"
            $launch = Invoke-ADB (@('-s', $start.serial, 'shell') + @($launchCommand)) 90
            $trial.first_launch = Parse-ActivityLaunch ($launch.stdout + "`n" + $launch.stderr) $launch.elapsed_ms
            if ($launch.exit_code -ne 0 -or $trial.first_launch.status -ne 'ok' -or !$trial.first_launch.complete) { Add-Failure $trial 'first_launch' ($launch.stderr + $launch.stdout) $launch.exit_code $launch.elapsed_ms }
            $phase = 'connected_android_test'
            $beforeHashes = Get-XmlHashes
            $since = [datetime]::UtcNow
            $tests = Invoke-Gradle 'connected-tests' $start.serial 900
            [void](Assert-AdbServerStable $Lifecycle 'connectedDebugAndroidTest')
            $null = Assert-TargetOnly $start.serial
            $suite = Get-XmlSuiteSummary $since $beforeHashes
            $trial.connected_android_test = [pscustomobject]@{
                elapsed_ms = $tests.elapsed_ms; exit_code = $tests.exit_code; outcome = if ($tests.exit_code -eq 0 -and !$tests.timed_out) { 'SUCCESS' } else { 'FAILED' }
                tests = if ($suite) { $suite.tests } else { $null }; failures = if ($suite) { $suite.failures } else { $null }; errors = if ($suite) { $suite.errors } else { $null }; skipped = if ($suite) { $suite.skipped } else { $null }
                reported_suite_seconds = if ($suite) { $suite.reported_suite_seconds } else { $null }; reports = if ($suite) { $suite.files } else { @() }; output_excerpt = if ($tests.exit_code -ne 0) { Get-Excerpt ($tests.stderr + "`n" + $tests.stdout) } else { $null }
            }
            if ($trial.connected_android_test.outcome -ne 'SUCCESS') { Add-Failure $trial 'connected_android_test' ($tests.stderr + $tests.stdout) $tests.exit_code $tests.elapsed_ms }
            if ($null -eq $suite) { Add-Failure $trial 'test_reports' 'no fresh connected test XML report found' $tests.exit_code $tests.elapsed_ms }
        }
    } catch {
        $trial.notes += $_.Exception.Message
        Add-Failure $trial $phase $_.Exception.Message $null $null
    } finally {
        $stop = Stop-Target $start.serial
        $trial.stop_ok = $stop.success
        if (!$stop.success) { Add-Failure $trial 'stop' ($stop.stderr + $stop.stdout) $stop.exit_code $null }
    }
    $trial.success = ($trial.reset_ok -and $start.exit_code -eq 0 -and $trial.install_debug.outcome -eq 'SUCCESS' -and $trial.first_launch.status -eq 'ok' -and $trial.first_launch.complete -and $trial.connected_android_test.outcome -eq 'SUCCESS' -and $trial.connected_android_test.failures -eq 0 -and $trial.connected_android_test.errors -eq 0 -and $trial.stop_ok)
    return [pscustomobject]@{ trial = $trial; abort = !$trial.stop_ok }
}

function Get-ExternalInventory([string]$Root) { return Get-ProcessInventorySnapshot $Root }

function Get-PageSize([string]$Serial) {
    $page = Invoke-ADB @('-s', $Serial, 'shell', 'getconf', 'PAGE_SIZE')
    $property = Invoke-ADB @('-s', $Serial, 'shell', 'getprop', 'ro.build.page_size')
    return [pscustomobject]@{ getconf_page_size = $page.stdout.Trim(); ro_build_page_size = $property.stdout.Trim(); available = (($page.stdout.Trim() -match '^\d+$') -or ($property.stdout.Trim() -match '^\d+$')) }
}

function Get-EnvironmentMetadata($Runtime, $HostMetadata, $Build, $Fixture, $Adb, $ExternalBefore, $PageSize) {
    $Build | Add-Member -NotePropertyName fixture_compile_sdk -NotePropertyValue 34 -Force
    $Build | Add-Member -NotePropertyName fixture_build_tools -NotePropertyValue '34.0.0' -Force
    $Build | Add-Member -NotePropertyName fixture_agp -NotePropertyValue '8.5.0' -Force
    $Build | Add-Member -NotePropertyName fixture_gradle -NotePropertyValue '8.7' -Force
    $Build | Add-Member -NotePropertyName fixture_kotlin -NotePropertyValue '1.9.22' -Force
    $Build | Add-Member -NotePropertyName fixture_repository -NotePropertyValue $Fixture.repository -Force
    $Build | Add-Member -NotePropertyName fixture_commit -NotePropertyValue $Fixture.commit -Force
    $Runtime.page_size = $PageSize
    return [ordered]@{
        generated_utc = [datetime]::UtcNow.ToString('o')
        emutrim_commit = (& git -C $script:Repo rev-parse HEAD).Trim()
        emutrim_branch = (& git -C $script:Repo rev-parse --abbrev-ref HEAD).Trim()
        emutrim_version = '0.7.0'
        host = $HostMetadata
        runtime = $Runtime
        build = $Build
        fixture = $Fixture
        adb_server = $Adb
        external_sdk_before = $ExternalBefore
        external_sdk_after = (Get-ExternalInventory $script:ExternalSdkRoot)
        external_sdk_modified = $false
    }
}

function Write-Summary($Raw, [string]$Path) {
    $lines = [System.Collections.Generic.List[string]]::new()
    $lines.Add('# Windows product-workflow benchmark')
    $lines.Add('')
    $lines.Add("Date: $([datetime]::Parse($Raw.generated_utc).ToString('yyyy-MM-dd')). EmuTrim $($Raw.emutrim_version) at ``$($Raw.emutrim_commit)``. Fixture ``$($Raw.fixture_commit)``. Status: **$($Raw.status)**.")
    $lines.Add('')
    $lines.Add('Control is the same managed AVD and launch path with `--no-slim`; Slim removes that flag. Both cold boot, use same RAM/image, and reset with `-wipe-data` before each trial. Gradle orchestration is included in install/test wall time.')
    $lines.Add('')
    $lines.Add('Negative paired delta means lower under Slim. Percent is `(Slim - Control) / Control * 100`. N=5 per condition; descriptive only.')
    $lines.Add('')
    $lines.Add('| Metric | Control raw | Control N / median / mean / min / max | Slim raw | Slim N / median / mean / min / max | Per-pair Slim − Control | Median paired delta | Mean paired delta | Median % |')
    $lines.Add('|---|---|---|---|---|---|---:|---:|---:|---:|')
    $metrics = @('boot_complete_ms','start_total_ms','slim_overhead_ms','idle_cpu_median','idle_working_set_median','idle_private_median','install_debug_ms','first_launch_this_time_ms','first_launch_total_time_ms','first_launch_wait_time_ms','connected_test_wall_ms','connected_test_suite_seconds')
    foreach ($metric in $metrics) {
        $controls = @($Raw.trials | Where-Object condition -eq 'CONTROL' | ForEach-Object { Get-MetricValue $_ $metric } | Where-Object { $null -ne $_ })
        $slims = @($Raw.trials | Where-Object condition -eq 'SLIM' | ForEach-Object { Get-MetricValue $_ $metric } | Where-Object { $null -ne $_ })
        $cs = Get-Statistics $controls; $ss = Get-Statistics $slims
        $pairs = @()
        foreach ($pair in 1..5) {
            $c = $Raw.trials | Where-Object { $_.pair -eq $pair -and $_.condition -eq 'CONTROL' } | Select-Object -First 1
            $s = $Raw.trials | Where-Object { $_.pair -eq $pair -and $_.condition -eq 'SLIM' } | Select-Object -First 1
            if ($null -ne $c -and $null -ne $s) {
                $cv = Get-MetricValue $c $metric; $sv = Get-MetricValue $s $metric
                if ($null -ne $cv -and $null -ne $sv) { $pairs += [pscustomobject]@{ pair=$pair; delta=[double]$sv-[double]$cv; pct=if([double]$cv -ne 0){([double]$sv-[double]$cv)/[double]$cv*100}else{$null} } }
            }
        }
        $ds = Get-Statistics ([double[]]@($pairs | ForEach-Object delta))
        $ps = Get-Statistics ([double[]]@($pairs | Where-Object { $null -ne $_.pct } | ForEach-Object pct))
        $rawC = ($cs.raw -join ', '); $rawS = ($ss.raw -join ', ')
        $summaryC = if ($cs.n) { "$($cs.n) / $([math]::Round($cs.median,3)) / $([math]::Round($cs.mean,3)) / $([math]::Round($cs.min,3)) / $([math]::Round($cs.max,3))" } else { '0 / n/a' }
        $summaryS = if ($ss.n) { "$($ss.n) / $([math]::Round($ss.median,3)) / $([math]::Round($ss.mean,3)) / $([math]::Round($ss.min,3)) / $([math]::Round($ss.max,3))" } else { '0 / n/a' }
        $pairText = ($pairs | ForEach-Object { "p$($_.pair):$([math]::Round($_.delta,3))" }) -join ', '
        $lines.Add("| ``$metric`` | $rawC | $summaryC | $rawS | $summaryS | $pairText | $([math]::Round($ds.median,3)) | $([math]::Round($ds.mean,3)) | $([math]::Round($ps.median,3))% |")
    }
    $lines.Add('')
    $lines.Add('## Reliability')
    $lines.Add('')
    $lines.Add('| Condition | starts | installs | first launches | connected tests | clean stops | workflow successes |')
    $lines.Add('|---|---:|---:|---:|---:|---:|---:|')
    foreach ($condition in @('CONTROL','SLIM')) {
        $rows = @($Raw.trials | Where-Object condition -eq $condition)
        $lines.Add("| $condition | $(@($rows | Where-Object { $_.start.exit_code -eq 0 }).Count)/5 | $(@($rows | Where-Object { $_.install_debug.outcome -eq 'SUCCESS' }).Count)/5 | $(@($rows | Where-Object { $_.first_launch.status -eq 'ok' -and $_.first_launch.complete }).Count)/5 | $(@($rows | Where-Object { $_.connected_android_test.outcome -eq 'SUCCESS' -and $_.connected_android_test.failures -eq 0 -and $_.connected_android_test.errors -eq 0 }).Count)/5 | $(@($rows | Where-Object stop_ok).Count)/5 | $(@($rows | Where-Object success).Count)/5 |")
    }
    $lines.Add('')
    $lines.Add('## Host and runtime')
    $lines.Add('')
    $lines.Add("$($Raw.runtime.emulator_version); $($Raw.runtime.system_image); RAM $($Raw.runtime.configured_ram); page size $($Raw.runtime.page_size.getconf_page_size) bytes; WHPX: $($Raw.runtime.whpx -replace "`r?`n", ' ')")
    $lines.Add('')
    $lines.Add('Host-load snapshots are retained per trial in `raw.json`; no trial is dropped by host-load value. Interpret pair consistency and reliability with raw distributions. N=5 is descriptive, not significance evidence.')
    $lines.Add('')
    $lines.Add('Limitations: one Windows host, one x86_64 system image, one Android testing-samples Espresso app/test suite, five trials per condition; no macOS result; results may differ with production apps. Gradle orchestration is included in install and connected-test wall times. `am start -W` for this sample does not establish general app performance.')
    [System.IO.File]::WriteAllLines($Path, $lines, [System.Text.UTF8Encoding]::new($false))
}

function Write-FlatCsv([object[]]$Trials, [string]$Path, [string]$RunStatus = 'unknown') {
    $rows = [System.Collections.Generic.List[object]]::new()
    foreach ($trial in $Trials) { $rows.Add((Convert-TrialToCsv -Trial $trial -RunStatus $RunStatus)) }
    $csv = @($rows | ConvertTo-Csv -NoTypeInformation)
    [System.IO.File]::WriteAllLines($Path, $csv, [System.Text.UTF8Encoding]::new($false))
}

function Close-AdbServer($Lifecycle) {
    if (!$Lifecycle.started_by_harness) { return [pscustomobject]@{ stopped = $false; reason = 'server pre-existed' } }
    $devices = Get-AdbDevices
    $known = @($Lifecycle.benchmark_serials)
    $unrelated = @($devices.devices | Where-Object { $_.serial -notmatch '^emulator-\d+$' -or $_.serial -notin $known })
    if ($unrelated.Count -gt 0) { return [pscustomobject]@{ stopped = $false; reason = 'unrelated ADB transport remains'; devices = $devices.devices } }
    $executable = if ($Lifecycle.current_executable) { $Lifecycle.current_executable } else { Join-Path $script:ManagedSdk 'platform-tools\adb.exe' }
    $kill = Invoke-Captured $executable @('kill-server') $script:Repo @{} 30
    $closed = @(Get-ListenerOwners 5037).Count -eq 0
    return [pscustomobject]@{ stopped = ($kill.exit_code -eq 0 -and $closed); exit_code = $kill.exit_code; reason = if ($closed) { 'no transports remain' } else { 'listener remains' } }
}

function Invoke-Run([string]$RunMode, [string]$RunOutputDirectory) {
    Assert-FixtureAndSdk
    Test-Parsers | Out-Null
    if (!(Test-Path $script:AndroidCli)) { throw 'standalone Android CLI is missing' }
    if ([string]::IsNullOrWhiteSpace($script:JavaHome) -or !(Test-Path (Join-Path $script:JavaHome 'bin\java.exe'))) { throw 'selected Java home has no bin\java.exe' }
    if (!(Test-Path (Join-Path $script:BenchmarkRoot 'gradle-version.stdout.txt'))) { throw 'Gradle wrapper version preflight missing' }
    if (!(Test-Path $script:RuntimeRoot)) { throw 'dedicated managed runtime root missing' }
    $benchmarkOutputRoot = Join-Path $script:Repo 'benchmarks'
    $outputRoot = Join-Path $benchmarkOutputRoot 'windows-product-proof'
    if ($RunMode -eq 'Official' -and ![string]::IsNullOrWhiteSpace($RunOutputDirectory)) {
        $outputRoot = [System.IO.Path]::GetFullPath((Join-Path $script:Repo $RunOutputDirectory))
        if (!$outputRoot.StartsWith($benchmarkOutputRoot + [System.IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
            throw 'official output directory must be below the repository benchmarks directory'
        }
    }
    if ($RunMode -eq 'Official' -and (Test-Path $outputRoot)) { throw "refusing existing result directory: $outputRoot" }
    if ($RunMode -eq 'Official') {
        $expectedHarnessHash = (Get-FileHash -LiteralPath $script:HarnessPath -Algorithm SHA256).Hash
        $dryRuns = @(Get-ChildItem $script:BenchmarkRoot -Filter 'dry-run-*.json' -File | ForEach-Object { try { Get-Content $_.FullName -Raw | ConvertFrom-Json } catch { $null } } | Where-Object { Test-CompletedDryRun $_ $expectedHarnessHash $null $true })
        if ($dryRuns.Count -eq 0) { throw 'Official requires a complete, successful control+Slim dry run' }
    }
    $externalBefore = Get-ExternalInventory $script:ExternalSdkRoot
    $externalStartJson = Join-Path $script:BenchmarkRoot 'external-sdk-before-official.json'
    if (!(Test-Path $externalStartJson)) { Write-JsonFile $externalBefore $externalStartJson }
    $adbLife = [ordered]@{ existed_before = $false; started_by_harness = $false; pid = $null; executable = $null; current_executable = $null; last_snapshot = $null; transitions = @(); start_server = $null; devices_before_trials = $null; cleanup = $null; benchmark_serials = @() }
    $hostMetadata = $null
    $runtimeMetadata = $null
    $buildMetadata = $null
    $fixtureMetadata = $null
    $warmups = @()
    $trials = @()
    $failures = @()
    $raw = $null
    $rawPath = $null
    $envMeta = $null
    $official = ($RunMode -eq 'Official')
    $idleSeconds = if ($RunMode -in @('Official', 'DryRun')) { 120 } else { 5 }
    $idleCount = if ($RunMode -in @('Official', 'DryRun')) { 5 } else { 1 }
    $runStamp = [datetime]::UtcNow.ToString('yyyyMMddTHHmmssZ')
    $runWork = Join-Path $script:BenchmarkRoot ("logs-$($RunMode.ToLowerInvariant())-$runStamp")
    try {
        if (Test-Path $runWork) { throw "run log directory already exists: $runWork" }
        New-Item -ItemType Directory -Path $runWork | Out-Null
        Start-AdbServer $adbLife
        $hostMetadata = Get-HostSnapshot
        $runtimeMetadata = Get-RuntimeMetadata
        $script:EmulatorBinaryHash = $runtimeMetadata.emulator_binary_sha256
        $buildMetadata = Get-BuildSdkMetadata
        $fixtureMetadata = Get-FixtureMetadata
        if ($RunMode -eq 'DryRun') {
            $dryPath = Join-Path $script:BenchmarkRoot "dry-run-$runStamp.json"
            $rawPath = $dryPath
            if (Test-Path $dryPath) { throw "dry-run result exists: $dryPath" }
            $raw = Get-ResultObject @() @() @() 'dry_run_in_progress'
            $raw.host = $hostMetadata; $raw.runtime = $runtimeMetadata; $raw.adb_server = $adbLife
            $raw.protocol.stabilization_seconds = $idleSeconds; $raw.protocol.idle_sample_count = $idleCount
            Write-JsonFile $raw $dryPath
        }
        foreach ($condition in @('CONTROL','SLIM')) {
            $warm = Invoke-Warmup $condition
            $warmups += $warm
            if ($warm.serial) { $adbLife.benchmark_serials += $warm.serial }
            if ($RunMode -eq 'DryRun') { $raw.warmups = $warmups; $raw.adb_server = $adbLife }
            if (!$warm.reset_ok -or !$warm.start_ok -or !$warm.health_ok -or !$warm.stop_ok) {
                $raw.status = 'failed_warmup'
                $raw.failures += [pscustomobject]@{ condition=$condition; pair=$null; trial='warmup'; phase=$warm.failure_phase; exit_code=$warm.exit_code; error=$warm.failure; elapsed_ms=$warm.elapsed_ms }
                if ($RunMode -eq 'DryRun') { Write-JsonFile $raw $dryPath }
                throw "warm-up failed for ${condition}: $($warm.failure)"
            }
            if ($RunMode -eq 'DryRun') { Write-JsonFile $raw $dryPath }
        }
        $adbLife.devices_before_trials = Get-AdbDevices
        $runtimeMetadata.page_size = $warmups[-1].page_size
        $envMeta = Get-EnvironmentMetadata $runtimeMetadata $hostMetadata $buildMetadata $fixtureMetadata $adbLife $externalBefore $runtimeMetadata.page_size
        if ($RunMode -eq 'DryRun') {
            $raw.status = 'dry_run'
            foreach ($order in @(Get-ConditionOrder | Select-Object -First 2)) {
                $result = Invoke-Trial $order $order.order $idleSeconds $idleCount $runWork $adbLife
                $trials += $result.trial
                $raw.trials = $trials
                if ($result.abort) { $raw.status = 'invalid_dry_run'; Write-JsonFile $raw $dryPath; throw 'dry run aborted after reset/stop failure' }
                if ($trials[-1].failures.Count -gt 0) { $raw.status = 'failed_dry_run'; Write-JsonFile $raw $dryPath; throw "dry run recorded failure in $($trials[-1].failures[0].phase): $($trials[-1].failures[0].error)" }
                Write-JsonFile $raw $dryPath
            }
            $raw.status = 'dry_run_complete'; Write-JsonFile $raw $dryPath
            "Dry run complete: $dryPath"
            return
        }
        New-Item -ItemType Directory -Path $outputRoot | Out-Null
        $rawPath = Join-Path $outputRoot 'raw.json'
        $csvPath = Join-Path $outputRoot 'raw.csv'
        $envPath = Join-Path $outputRoot 'environment.json'
        $summaryPath = Join-Path $outputRoot 'summary.md'
        $raw = Get-ResultObject @() $warmups @() 'in_progress'
        $raw.host = $hostMetadata; $raw.runtime = $runtimeMetadata
        $raw.adb_server = $adbLife
        Write-JsonFile $raw $rawPath
        Write-JsonFile $envMeta $envPath
        foreach ($order in Get-ConditionOrder) {
            $result = Invoke-Trial $order ([int](($order.pair - 1) * 2 + $order.order)) $idleSeconds $idleCount $runWork $adbLife
            $trials += $result.trial
            if ($result.trial.serial) { $adbLife.benchmark_serials += $result.trial.serial }
            $raw.trials = $trials
            $raw.adb_server = $adbLife
            $raw.failures = @($trials | ForEach-Object failures)
            if ($result.abort) { $raw.status = 'invalid_run_aborted'; Write-JsonFile $raw $rawPath; Write-FlatCsv $trials $csvPath 'aborted'; throw 'official run aborted after reset or stop failure; raw partial results retained' }
            Write-JsonFile $raw $rawPath
            Write-FlatCsv $trials $csvPath 'in_progress'
        }
        $raw.status = 'complete'
        Write-JsonFile $raw $rawPath
        Write-FlatCsv $trials $csvPath 'complete'
        Write-Summary $raw $summaryPath
        $after = Get-ExternalInventory $script:ExternalSdkRoot
        $envMeta.external_sdk_after = $after
        $envMeta.external_sdk_modified = ((ConvertTo-Json $envMeta.external_sdk_before -Compress) -ne (ConvertTo-Json $after -Compress))
        Write-JsonFile $envMeta $envPath
        if ($envMeta.external_sdk_modified) { throw 'external Android Studio SDK inventory changed; benchmark validity failed' }
        "Official benchmark complete: $outputRoot"
    } finally {
        try { $adbLife.cleanup = Close-AdbServer $adbLife } catch { $adbLife.cleanup = [pscustomobject]@{ stopped = $false; error = $_.Exception.Message } }
        if ($null -ne $raw) {
            $raw.adb_server = $adbLife
            if ($null -ne $rawPath) { Write-JsonFile $raw $rawPath }
        }
        if ($null -ne $envMeta) {
            $envMeta.adb_server = $adbLife
            $envMeta.external_sdk_after = Get-ExternalInventory $script:ExternalSdkRoot
            $envMeta.external_sdk_modified = ((ConvertTo-Json $envMeta.external_sdk_before -Compress) -ne (ConvertTo-Json $envMeta.external_sdk_after -Compress))
            if ($RunMode -eq 'Official') {
                $envPath = Join-Path $outputRoot 'environment.json'
                if (Test-Path (Split-Path $envPath -Parent)) { Write-JsonFile $envMeta $envPath }
            } elseif ($RunMode -eq 'DryRun' -and $null -ne $rawPath) {
                Write-JsonFile $envMeta ([System.IO.Path]::ChangeExtension($rawPath, '.environment.json'))
            }
        }
    }
}

if ($Mode -eq 'ExportPartial') {
    if ([string]::IsNullOrWhiteSpace($InputRawPath) -or [string]::IsNullOrWhiteSpace($CsvPath)) { throw 'ExportPartial requires -InputRawPath and -CsvPath' }
    $source = [System.IO.Path]::GetFullPath($InputRawPath)
    $destination = [System.IO.Path]::GetFullPath($CsvPath)
    if ($source -eq $destination) { throw 'refusing to overwrite raw JSON with CSV output' }
    $raw = Get-Content -LiteralPath $source -Raw | ConvertFrom-Json
    if ($raw.status -notin @('invalid_run_aborted','aborted')) { throw "refusing partial export for status '$($raw.status)'" }
    Write-FlatCsv -Trials @($raw.trials) -Path $destination -RunStatus 'aborted'
    "Diagnostic CSV exported: $destination (run_status=aborted; trials=$($raw.trials.Count))"
    exit 0
}
Invoke-Run $Mode $OutputDirectory
