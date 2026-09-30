# Install Relay as a background service for the current user on Windows.
#
#   powershell -ExecutionPolicy Bypass -File .\install.ps1
#
# Must be run from an elevated PowerShell. Initializes this device if needed,
# then `relay service install` copies the exe to
# %LOCALAPPDATA%\Programs\Relay, adds that folder to the user PATH, registers
# the startup task and firewall rule, and starts the service.
$ErrorActionPreference = 'Stop'

$source = Join-Path $PSScriptRoot 'relay.exe'
if (-not (Test-Path $source)) {
    throw "relay.exe not found next to install.ps1 ($source)"
}

$principal = New-Object Security.Principal.WindowsPrincipal(
    [Security.Principal.WindowsIdentity]::GetCurrent())
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this from an elevated PowerShell (right-click, Run as administrator)."
}

# Downloaded files carry a "mark of the web"; clear it so the exe can run.
Unblock-File $source

& $source id 2>&1 | Out-Null
if ($LASTEXITCODE -ne 0) {
    $name = $env:COMPUTERNAME.ToLowerInvariant()
    & $source init --name $name
    if ($LASTEXITCODE -ne 0) { throw "relay init failed" }
}

& $source service install
if ($LASTEXITCODE -ne 0) { throw "relay service install failed" }

Write-Host "Installed. Open a new terminal so 'relay' is on your PATH."
