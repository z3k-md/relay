# Install relay.exe for the current user on Windows.
#
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#
# Copies relay.exe to %LOCALAPPDATA%\Programs\Relay and adds that folder to
# your user PATH. Open a new terminal afterwards so PATH takes effect.
$ErrorActionPreference = 'Stop'

$source = Join-Path $PSScriptRoot 'relay.exe'
if (-not (Test-Path $source)) {
    throw "relay.exe not found next to install.ps1 ($source)"
}

$dest = Join-Path $env:LOCALAPPDATA 'Programs\Relay'
New-Item -ItemType Directory -Force -Path $dest | Out-Null
Copy-Item $source (Join-Path $dest 'relay.exe') -Force
# Downloaded files carry a "mark of the web"; clear it so SmartScreen does not
# block every run.
Unblock-File (Join-Path $dest 'relay.exe')

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$parts = @()
if ($userPath) { $parts = $userPath.Split(';') | Where-Object { $_ -ne '' } }
if ($parts -notcontains $dest) {
    [Environment]::SetEnvironmentVariable('Path', (($parts + $dest) -join ';'), 'User')
    Write-Host "Added $dest to your user PATH (open a new terminal)."
}

Write-Host "Installed $dest\relay.exe"
Write-Host ""
Write-Host "Relay listens on UDP port 47321. The first time you run 'relay run',"
Write-Host "Windows asks whether to allow it through the firewall: allow Private networks."
Write-Host "To add the rule up front instead, run this in an elevated PowerShell:"
Write-Host "  New-NetFirewallRule -DisplayName 'Relay' -Direction Inbound -Protocol UDP -LocalPort 47321 -Program '$dest\relay.exe' -Action Allow -Profile Private"
