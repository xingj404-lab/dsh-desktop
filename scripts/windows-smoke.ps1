param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerPath,
    [string]$LogDirectory = "$env:RUNNER_TEMP\dsh-windows-smoke"
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
$appProcess = $null
$backend = $null

New-Item -ItemType Directory -Force -Path $LogDirectory | Out-Null

function Save-Diagnostics {
    Get-CimInstance Win32_Process |
        Where-Object { $_.Name -in @("dsh-desktop.exe", "DeepSeek Harness.exe", "node.exe") } |
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

function Get-VerifiedBackendProcess {
    param(
        [uint32]$BackendProcessId,
        [uint32]$ParentProcessId,
        [string]$ExpectedInstallRoot
    )
    $candidate = Get-CimInstance Win32_Process -Filter "ProcessId = $BackendProcessId" `
        -ErrorAction SilentlyContinue
    if (-not $candidate -or -not $candidate.ExecutablePath) {
        return $null
    }
    $nodePath = [System.IO.Path]::GetFullPath($candidate.ExecutablePath)
    if (
        $candidate.Name -eq "node.exe" -and
        $candidate.ParentProcessId -eq $ParentProcessId -and
        $candidate.CommandLine -match "@deepseek-ai[\\/]dsh[\\/]lib[\\/]bin\.js" -and
        $candidate.CommandLine -match "\sweb\s" -and
        $nodePath.StartsWith($ExpectedInstallRoot, [System.StringComparison]::OrdinalIgnoreCase)
    ) {
        return $candidate
    }
    return $null
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
        $appError = Get-Content $stderrLog -Raw -ErrorAction SilentlyContinue
        throw "Bundled dsh backend did not start within 60 seconds. App stderr: $appError Observed processes: $observed"
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
        $backend = Get-VerifiedBackendProcess -BackendProcessId $backend.ProcessId `
            -ParentProcessId $appProcess.Id -ExpectedInstallRoot $installRoot
        if (-not $backend) {
            Start-Sleep -Milliseconds 250
            $appError = Get-Content $stderrLog -Raw -ErrorAction SilentlyContinue
            throw "Bundled dsh backend exited before becoming ready. App stderr: $appError"
        }
        try {
            $response = Invoke-WebRequest -Uri $url -UseBasicParsing -TimeoutSec 3
            $ready = $response.StatusCode -eq 200 -and $response.Content.Length -gt 0
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
    $backend = Get-VerifiedBackendProcess -BackendProcessId $backend.ProcessId `
        -ParentProcessId $appProcess.Id -ExpectedInstallRoot $installRoot
    if (-not $backend) {
        throw "Bundled dsh backend exited during the startup soak"
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
    if ($backend -and $appProcess -and $installRoot) {
        $verifiedBackend = Get-VerifiedBackendProcess -BackendProcessId $backend.ProcessId `
            -ParentProcessId $appProcess.Id -ExpectedInstallRoot $installRoot
        if ($verifiedBackend) {
            Stop-Process -Id $verifiedBackend.ProcessId -Force -ErrorAction SilentlyContinue
        }
    }
    if ($appProcess -and -not $appProcess.HasExited) {
        Stop-Process -Id $appProcess.Id -Force -ErrorAction SilentlyContinue
    }
}
