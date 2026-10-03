param([Parameter(Mandatory=$true)][string]$BenchmarkRoot,[Parameter(Mandatory=$true)][string]$EmuTrimExe)
$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root=Join-Path $BenchmarkRoot 'managed'
$sdk=Join-Path $root 'sdk'
$env:EMUTRIM_HOME=Split-Path $root -Parent
$env:ANDROID_HOME=$sdk
$env:ANDROID_SDK_ROOT=$sdk
$env:ANDROID_AVD_HOME=Join-Path $root 'avd'
$env:ANDROID_USER_HOME=Join-Path $root 'tmp\android-user'
$env:ANDROID_EMULATOR_HOME=Join-Path $root 'tmp\emulator-home'
$env:TMPDIR=Join-Path $root 'tmp'
$emu=Join-Path $sdk 'emulator\emulator.exe'
$adb=Join-Path $sdk 'platform-tools\adb.exe'
$exe=$EmuTrimExe
$avd='EmuTrim_Managed'
$serial='emulator-5554'
$port=5554
$out=Join-Path $PSScriptRoot 'stable-endurance.jsonl'
function Save-Record($Record){ Add-Content -LiteralPath $out -Value (ConvertTo-Json -InputObject $Record -Depth 10 -Compress) -Encoding utf8 }
function Get-HostSample {
  $os=Get-CimInstance Win32_OperatingSystem
  $committed=$null;$limit=$null
  try {$c=Get-Counter '\Memory\Committed Bytes','\Memory\Commit Limit';foreach($s in $c.CounterSamples){if($s.Path -match 'committed bytes$'){$committed=[long]$s.CookedValue};if($s.Path -match 'commit limit$'){$limit=[long]$s.CookedValue}}}catch{}
  $procs=@(Get-Process emulator,qemu-system-x86_64,qemu-system-aarch64 -ErrorAction SilentlyContinue | Select-Object ProcessName,Id,Handles,WorkingSet64,PrivateMemorySize64)
  $hv=$null;try{$hv=(Get-Counter '\Hyper-V Hypervisor Partition(*)\Virtual Processors','\Hyper-V Hypervisor Partition(*)\GPA Pages','\Hyper-V Hypervisor\Partitions','\Hyper-V Hypervisor\Virtual Processors' -ErrorAction Stop).CounterSamples|Select-Object Path,CookedValue}catch{}
  [pscustomobject]@{utc=[datetime]::UtcNow.ToString('o');free_physical_bytes=[long]$os.FreePhysicalMemory*1KB;committed_bytes=$committed;commit_limit_bytes=$limit;emulator_processes=$procs;hyperv_counter=$hv}
}
function Test-Port([int]$p){$c=[Net.Sockets.TcpClient]::new();try{$a=$c.BeginConnect('127.0.0.1',$p,$null,$null);if(!$a.AsyncWaitHandle.WaitOne(300)) {return $false};$c.EndConnect($a);return $true}catch{return $false}finally{$c.Dispose()}}
function Get-ManagedProcesses { @(Get-CimInstance Win32_Process -Filter "name='emulator.exe' OR name LIKE 'qemu-system-%'" | Where-Object { $_.ExecutablePath -like "$sdk*" -or $_.CommandLine -like "*$avd*" }) }
if(!(Test-Path $emu) -or !(Test-Path $adb) -or !(Test-Path -LiteralPath $exe -PathType Leaf)){throw 'required managed Emulator, ADB, or selected EmuTrim executable missing'}
$emulatorVersion=(& $emu -version 2>&1 | Select-Object -First 1).ToString().Trim()
if($emulatorVersion -notmatch '^Android emulator version 37\.1\.11\.0 \(build_id 15917651\)(?: \(CL:[^)]+\))?$'){throw "stable endurance requires Emulator 37.1.11.0 (build_id 15917651); selected root reports: $emulatorVersion"}
if((Get-Process emulator,qemu-system-x86_64,qemu-system-aarch64 -ErrorAction SilentlyContinue)){throw 'emulator/QEMU already running; refusing series'}
if(Test-Port 5037){throw 'ADB listener already exists; refusing to interfere with another server'}
if(Test-Port $port){throw 'console port already occupied'}
if(Test-Path $out){throw "diagnostic evidence already exists; refusing overwrite: $out"}
Save-Record ([pscustomobject]@{type='series_start';utc=[datetime]::UtcNow.ToString('o');host=(Get-HostSample);emulator_version=$emulatorVersion;accel_check=(& $emu -accel-check 2>&1 | Out-String).Trim();argv=@('-avd',$avd,'-port',"$port",'-gpu','host','-memory','4096','-wipe-data','-no-snapshot-load','-no-window')})
for($n=1;$n -le 25;$n++){
  if(Test-Port 5037 -or (Get-ManagedProcesses).Count -gt 0 -or Test-Port $port){throw "pre-attempt stopped-state proof failed at attempt $n"}
  $sample=Get-HostSample
  $base=Join-Path $PSScriptRoot ('stable-{0:D2}' -f $n)
  $proc=$null;$failure=$null;$console=$false;$adbSeen=$false;$qemu=$false;$boot=$false;$launchWatch=[Diagnostics.Stopwatch]::StartNew()
$args=@('-avd',$avd,'-port',"$port",'-gpu','host','-datadir',(Join-Path $env:ANDROID_AVD_HOME "$avd.avd"),'-memory','4096','-wipe-data','-no-snapshot-load','-no-window','-verbose')
  try{
    $proc=Start-Process -FilePath $emu -ArgumentList $args -WorkingDirectory (Split-Path $emu) -WindowStyle Hidden -PassThru -RedirectStandardOutput "$base.stdout.log" -RedirectStandardError "$base.stderr.log"
    $consoleWatch=[Diagnostics.Stopwatch]::StartNew();while($consoleWatch.Elapsed.TotalSeconds -lt 180 -and !$console){$console=Test-Port $port;if($proc.HasExited){throw "Emulator exited before console (exit $($proc.ExitCode))"};Start-Sleep -Milliseconds 250};if(!$console){throw 'console timeout'}
    $qemu=(Get-ManagedProcesses).Count -gt 0
    $adbWatch=[Diagnostics.Stopwatch]::StartNew();while($adbWatch.Elapsed.TotalSeconds -lt 180 -and !$adbSeen){$devices=& $adb devices 2>$null;$adbSeen=@($devices|Where-Object {$_ -match "^$serial\s+device\b"}).Count -eq 1;if(!$adbSeen -and $proc.HasExited){throw "Emulator exited before ADB (exit $($proc.ExitCode))"};if(!$adbSeen){Start-Sleep -Seconds 1}}
    if(!$adbSeen){throw 'exact ADB device timeout'}
    $bootWatch=[Diagnostics.Stopwatch]::StartNew();while($bootWatch.Elapsed.TotalSeconds -lt 360 -and !$boot){$v=& $adb -s $serial shell getprop sys.boot_completed 2>$null;$boot=($LASTEXITCODE -eq 0 -and "$v".Trim() -eq '1');if(!$boot){Start-Sleep -Seconds 2}}
    if(!$boot){throw 'boot_completed timeout'}
  }catch{$failure=$_.Exception.Message}
  $launchWatch.Stop()
  $exitCode=if($proc -and $proc.HasExited){$proc.ExitCode}else{$null}
  $qemuAtFailure=(Get-ManagedProcesses).Count -gt 0
  $stderr=if(Test-Path "$base.stderr.log"){@(Select-String -Path "$base.stderr.log" -Pattern 'WHPX|Failed to setup partition|failed to initialize WHPX|ERROR|FATAL' | Select-Object -Last 25 | ForEach-Object Line)}else{@()}
  Save-Record ([pscustomobject]@{type='attempt';attempt=$n;utc=[datetime]::UtcNow.ToString('o');elapsed_ms=$launchWatch.ElapsedMilliseconds;success=($null -eq $failure -and $console -and $adbSeen -and $qemu -and $boot);failure=$failure;exit_code=$exitCode;console_appeared=$console;adb_appeared=$adbSeen;qemu_appeared=($qemu -or $qemuAtFailure);processes_at_failure=(Get-ManagedProcesses);host_before=$sample;host_at_failure=if($failure){Get-HostSample}else{$null};boot_completed=$boot;whpx_log=$stderr;stdout_log="$base.stdout.log";stderr_log="$base.stderr.log"})
  if($null -ne $failure){break}
  $stop=& $exe stop $serial 2>&1;$stopExit=$LASTEXITCODE
  if($stopExit -ne 0){Save-Record ([pscustomobject]@{type='stop_failure';attempt=$n;exit_code=$stopExit;output=@($stop)});break}
  $stopWatch=[Diagnostics.Stopwatch]::StartNew();while($stopWatch.Elapsed.TotalSeconds -lt 60 -and $proc -and !$proc.HasExited){Start-Sleep -Milliseconds 250};if($proc -and !$proc.HasExited){try{$proc.Refresh()}catch{}}
  $consoleStopOutput=@()
  if($proc -and !$proc.HasExited){$consoleStopOutput=@(& $adb -s $serial emu kill 2>$null);$wait=[Diagnostics.Stopwatch]::StartNew();while($wait.Elapsed.TotalSeconds -lt 60 -and !$proc.HasExited){Start-Sleep -Milliseconds 250;try{$proc.Refresh()}catch{}}}
  $processGone=($proc -and $proc.HasExited -and (Get-ManagedProcesses).Count -eq 0)
  if(Test-Port 5037){& $adb kill-server 2>$null|Out-Null}
  $clean=($processGone -and !(Test-Port $port) -and !(Test-Port 5037) -and (Get-ManagedProcesses).Count -eq 0)
  Save-Record ([pscustomobject]@{type='stop';attempt=$n;utc=[datetime]::UtcNow.ToString('o');process_exit=($proc -and $proc.HasExited);exit_code=$(if($proc -and $proc.HasExited){$proc.ExitCode}else{$null});process_and_helper_absent=((Get-ManagedProcesses).Count -eq 0);console_absent=!(Test-Port $port);adb_listener_absent=!(Test-Port 5037);clean=$clean;product_stop_output=@($stop);console_stop_output=$consoleStopOutput})
  if(!$clean){Save-Record ([pscustomobject]@{type='series_failure';attempt=$n;reason='stop/process/listener/ADB absence proof failed'});break}
}
$records=@(Get-Content $out|ConvertFrom-Json)
$failed=@($records|Where-Object {$_.type -eq 'attempt' -and !$_.success})
$accelAfter=$null
if($failed.Count){
  $accelAfter=(& $emu -accel-check 2>&1|Out-String).Trim()
  Save-Record ([pscustomobject]@{type='accel_check_after_failure';utc=[datetime]::UtcNow.ToString('o');result=$accelAfter})
  if(!(Test-Port 5037) -and !(Test-Port $port) -and (Get-ManagedProcesses).Count -eq 0){
    $base=Join-Path $PSScriptRoot 'stable-followup'
    $proc=$null;$console=$false;$adbSeen=$false;$boot=$false;$errorText=$null;$w=[Diagnostics.Stopwatch]::StartNew()
    try{
      $args=@('-avd',$avd,'-port',"$port",'-gpu','host','-datadir',(Join-Path $env:ANDROID_AVD_HOME "$avd.avd"),'-memory','4096','-wipe-data','-no-snapshot-load','-no-window','-verbose')
      $proc=Start-Process -FilePath $emu -ArgumentList $args -WorkingDirectory (Split-Path $emu) -WindowStyle Hidden -PassThru -RedirectStandardOutput "$base.stdout.log" -RedirectStandardError "$base.stderr.log"
      $deadline=[Diagnostics.Stopwatch]::StartNew();while($deadline.Elapsed.TotalSeconds -lt 180 -and !$console){$console=Test-Port $port;if($proc.HasExited){break};Start-Sleep -Milliseconds 250}
      $adbDeadline=[Diagnostics.Stopwatch]::StartNew();while($console -and $adbDeadline.Elapsed.TotalSeconds -lt 180 -and !$adbSeen){$devices=& $adb devices 2>$null;$adbSeen=@($devices|Where-Object {$_ -match "^$serial\s+device\b"}).Count -eq 1;if(!$adbSeen -and $proc.HasExited){break};if(!$adbSeen){Start-Sleep -Seconds 1}}
      $bootDeadline=[Diagnostics.Stopwatch]::StartNew();while($adbSeen -and $bootDeadline.Elapsed.TotalSeconds -lt 360 -and !$boot){$v=& $adb -s $serial shell getprop sys.boot_completed 2>$null;$boot=($LASTEXITCODE -eq 0 -and "$v".Trim() -eq '1');if(!$boot){Start-Sleep -Seconds 2}}
    }catch{$errorText=$_.Exception.Message}
    $w.Stop();$followLog=if(Test-Path "$base.stderr.log"){@(Select-String -Path "$base.stderr.log" -Pattern 'WHPX|Failed to setup partition|failed to initialize WHPX|ERROR|FATAL' | Select-Object -Last 30 | ForEach-Object Line)}else{@()}
    Save-Record ([pscustomobject]@{type='single_verbose_followup';utc=[datetime]::UtcNow.ToString('o');elapsed_ms=$w.ElapsedMilliseconds;error=$errorText;exit_code=if($proc -and $proc.HasExited){$proc.ExitCode}else{$null};console_appeared=$console;adb_appeared=$adbSeen;boot_completed=$boot;qemu_appeared=((Get-ManagedProcesses).Count -gt 0);whpx_log=$followLog;stdout_log="$base.stdout.log";stderr_log="$base.stderr.log"})
    if($boot){$stop2=& $exe stop $serial 2>&1;$stop2Exit=$LASTEXITCODE;$stopWatch=[Diagnostics.Stopwatch]::StartNew();while($stopWatch.Elapsed.TotalSeconds -lt 60 -and !$proc.HasExited){Start-Sleep -Milliseconds 250;try{$proc.Refresh()}catch{}};if(Test-Port 5037){& $adb kill-server 2>$null|Out-Null};Save-Record ([pscustomobject]@{type='single_verbose_followup_stop';exit_code=$stop2Exit;process_exit=$proc.HasExited;process_and_listener_absent=((Get-ManagedProcesses).Count -eq 0 -and !(Test-Port $port) -and !(Test-Port 5037));output=@($stop2)})}
  }else{Save-Record ([pscustomobject]@{type='single_verbose_followup';skipped='prior launch left emulator process or listener active; preserve for reboot evidence'})}
}
Save-Record ([pscustomobject]@{type='series_end';utc=[datetime]::UtcNow.ToString('o');attempts=@($records|Where-Object type -eq 'attempt').Count;successes=@($records|Where-Object {$_.type -eq 'attempt' -and $_.success}).Count;first_failure=if($failed.Count){$failed[0].attempt}else{$null};records=$out;accel_check_after_failure=$accelAfter})
