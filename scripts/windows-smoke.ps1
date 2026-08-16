param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$LogDirectory = "$env:RUNNER_TEMP\dsh-windows-smoke"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$appProcess = $null
$backend = $null
$backendProbe = $null

New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null

function Save-Diagnostics {
    Get-CimInstance Win32_Process |
        Where-Object { $_.Name -in @("dsh-desktop.exe", "node.exe") } |
        Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine |
        ConvertTo-Json -Depth 3 |
        Set-Content -Path "$LogDirectory\processes.json"

    Get-WinEvent -FilterHashtable @{
        LogName   = "Application"
        StartTime = (Get-Date).AddMinutes(-20)
    } -ErrorAction SilentlyContinue |
        Where-Object { $_.Message -match "dsh-desktop|DeepSeek Harness|node.exe" } |
        Select-Object TimeCreated, ProviderName, Id, LevelDisplayName, Message |
        Format-List |
        Out-File -FilePath "$LogDirectory\application-events.txt" -Width 240

    $webViewKeys = @(
        "HKCU:\Software\Microsoft\EdgeUpdate\Clients\*",
        "HKLM:\Software\Microsoft\EdgeUpdate\Clients\*",
        "HKLM:\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\*"
    )
    Get-ItemProperty $webViewKeys -ErrorAction SilentlyContinue |
        Where-Object { $_.name -match "WebView2" } |
        Select-Object name, pv, location |
        Format-List |
        Out-File -FilePath "$LogDirectory\webview2.txt" -Width 240
}

function Find-InstalledApp {
    $uninstallKeys = @(
        "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*",
        "HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*",
        "HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*"
    )
    $entry = Get-ItemProperty $uninstallKeys -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName -eq "DeepSeek Harness" } |
        Select-Object -First 1

    if ($entry) {
        $entry | Select-Object DisplayName, DisplayVersion, InstallLocation, DisplayIcon |
            Format-List |
            Out-File -FilePath "$LogDirectory\installation.txt" -Width 240

        if ($entry.InstallLocation) {
            $installLocation = [Environment]::ExpandEnvironmentVariables(
                $entry.InstallLocation.Trim().Trim('"')
            )
            foreach ($exeName in @("DeepSeek Harness.exe", "dsh-desktop.exe")) {
                $candidate = Join-Path $installLocation $exeName
                if (Test-Path $candidate) {
                    return $candidate
                }
            }
        }
        if ($entry.DisplayIcon) {
            $candidate = ($entry.DisplayIcon -replace ',\d+$', '').Trim('"')
            if (Test-Path $candidate) {
                return $candidate
            }
        }
    }

    $roots = @(
        "$env:LOCALAPPDATA\DeepSeek Harness",
        "$env:LOCALAPPDATA\Programs\DeepSeek Harness",
        "$env:ProgramFiles\DeepSeek Harness"
    )
    foreach ($root in $roots) {
        foreach ($exeName in @("DeepSeek Harness.exe", "dsh-desktop.exe")) {
            $candidate = Join-Path $root $exeName
            if (Test-Path $candidate) {
                return $candidate
            }
        }
    }
    throw "DeepSeek Harness.exe was not found after installation"
}

try {
    $installer = (Resolve-Path $InstallerPath).Path
    $install = Start-Process -FilePath $installer -ArgumentList "/S" -Wait -PassThru
    if ($install.ExitCode -ne 0) {
        throw "NSIS installer exited with code $($install.ExitCode)"
    }

    $appExe = Find-InstalledApp
    Write-Host "Installed application: $appExe"

    $installRoot = [System.IO.Path]::GetFullPath((Split-Path $appExe))
    $bundledNode = Get-ChildItem $installRoot -Recurse -Filter "node.exe" |
        Where-Object { $_.FullName -match "[\\/]backend[\\/]node\.exe$" } |
        Select-Object -First 1
    $bundledBin = Get-ChildItem $installRoot -Recurse -Filter "bin.js" |
        Where-Object { $_.FullName -match "@deepseek-ai[\\/]dsh[\\/]lib[\\/]bin\.js$" } |
        Select-Object -First 1
    if (-not $bundledNode -or -not $bundledBin) {
        throw "Installed app is missing bundled node.exe or dsh/lib/bin.js under $installRoot"
    }

    $workspace = "$env:LOCALAPPDATA\ai.deepseek.harness.desktop\backend-workspace"
    New-Item -ItemType Directory -Force -Path $workspace | Out-Null
    # Rust's std::fs::canonicalize returns a verbatim (\\?\) path on Windows.
    # Match the application's Command::current_dir input exactly.
    $workspace = "\\?\$workspace"

    $listener = [System.Net.Sockets.TcpListener]::new(
        [System.Net.IPAddress]::Loopback,
        0
    )
    $listener.Start()
    $probePort = $listener.LocalEndpoint.Port
    $listener.Stop()
    $probeUrl = "http://127.0.0.1:$probePort"
    $probeStdout = Join-Path $LogDirectory "backend-probe-stdout.txt"
    $probeStderr = Join-Path $LogDirectory "backend-probe-stderr.txt"
    $probeArguments = @(
        "`"$($bundledBin.FullName)`"",
        "web",
        "--host", "127.0.0.1",
        "--port", "$probePort"
    )
    $backendProbe = Start-Process -FilePath $bundledNode.FullName `
        -ArgumentList $probeArguments -WorkingDirectory $workspace -PassThru `
        -RedirectStandardOutput $probeStdout -RedirectStandardError $probeStderr

    $probeReady = $false
    $probeDeadline = (Get-Date).AddSeconds(60)
    do {
        Start-Sleep -Seconds 1
        $backendProbe.Refresh()
        if ($backendProbe.HasExited) {
            $probeError = Get-Content $probeStderr -Raw -ErrorAction SilentlyContinue
            throw "Bundled backend probe exited with code $($backendProbe.ExitCode): $probeError"
        }
        try {
            $response = Invoke-WebRequest -Uri $probeUrl -UseBasicParsing -TimeoutSec 3
            $probeReady = $response.StatusCode -ge 200 -and $response.StatusCode -lt 500
        } catch {
            # The backend is still starting.
        }
    } while (-not $probeReady -and (Get-Date) -lt $probeDeadline)
    if (-not $probeReady) {
        $probeError = Get-Content $probeStderr -Raw -ErrorAction SilentlyContinue
        throw "Bundled backend probe did not respond at ${probeUrl}: $probeError"
    }
    Stop-Process -Id $backendProbe.Id -Force
    $backendProbe = $null

    $stdoutLog = Join-Path $LogDirectory "app-stdout.txt"
    $stderrLog = Join-Path $LogDirectory "app-stderr.txt"

    $appProcess = Start-Process -FilePath $appExe -PassThru `
        -RedirectStandardOutput $stdoutLog -RedirectStandardError $stderrLog
    $deadline = (Get-Date).AddSeconds(60)

    do {
        Start-Sleep -Seconds 1
        $appProcess.Refresh()
        if ($appProcess.HasExited) {
            throw "DeepSeek Harness exited during startup with code $($appProcess.ExitCode)"
        }

        $backend = Get-CimInstance Win32_Process -Filter "Name = 'node.exe'" |
            Where-Object {
                $nodePath = if ($_.ExecutablePath) {
                    [System.IO.Path]::GetFullPath($_.ExecutablePath)
                } else {
                    ""
                }
                $_.CommandLine -match "@deepseek-ai[\\/]dsh[\\/]lib[\\/]bin\.js" -and
                $_.CommandLine -match "\sweb\s" -and
                $_.ParentProcessId -eq $appProcess.Id -and
                $nodePath.StartsWith($installRoot, [System.StringComparison]::OrdinalIgnoreCase)
            } |
            Select-Object -First 1
    } while (-not $backend -and (Get-Date) -lt $deadline)

    if (-not $backend) {
        $observed = Get-CimInstance Win32_Process |
            Where-Object { $_.Name -in @("dsh-desktop.exe", "node.exe") } |
            Select-Object ProcessId, ParentProcessId, Name, ExecutablePath, CommandLine |
            Format-Table -AutoSize |
            Out-String
        throw "Bundled dsh backend did not start within 60 seconds. Observed processes: $observed"
    }

    $backend | Select-Object ProcessId, ParentProcessId, ExecutablePath, CommandLine |
        Format-List |
        Out-File -FilePath "$LogDirectory\backend.txt" -Width 240

    if ($backend.CommandLine -notmatch "--port\s+(\d+)") {
        throw "Could not read backend port from: $($backend.CommandLine)"
    }
    $port = [int]$Matches[1]
    $url = "http://127.0.0.1:$port"

    $ready = $false
    $deadline = (Get-Date).AddSeconds(60)
    do {
        try {
            $response = Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 3
            $ready = $response.StatusCode -ge 200 -and $response.StatusCode -lt 500
        } catch {
            Start-Sleep -Seconds 1
        }
    } while (-not $ready -and (Get-Date) -lt $deadline)

    if (-not $ready) {
        throw "Bundled dsh backend did not respond at $url"
    }

    Start-Sleep -Seconds 10
    $appProcess.Refresh()
    if ($appProcess.HasExited) {
        throw "DeepSeek Harness exited after the backend became ready"
    }

    "Application stayed alive and backend responded at $url" |
        Set-Content -Path "$LogDirectory\result.txt"
    Write-Host "Windows runtime smoke test passed: $url"
} catch {
    $failure = $_ | Out-String
    $failure | Set-Content -Path "$LogDirectory\failure.txt"
    Write-Host $failure
    throw
} finally {
    Save-Diagnostics
    if ($backendProbe -and -not $backendProbe.HasExited) {
        Stop-Process -Id $backendProbe.Id -Force -ErrorAction SilentlyContinue
    }
    if ($backend) {
        Stop-Process -Id $backend.ProcessId -Force -ErrorAction SilentlyContinue
    }
    if ($appProcess -and -not $appProcess.HasExited) {
        Stop-Process -Id $appProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
