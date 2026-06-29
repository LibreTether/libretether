# LibreTether agent installer for Windows.
#
# Relay mode:
#   & ([scriptblock]::Create((irm <base>/install-windows.ps1))) -Token TOKEN -Relay HOST:PORT -RelaySecret SECRET
#
# Direct / Tailscale mode:
#   & ([scriptblock]::Create((irm <base>/install-windows.ps1))) -Token TOKEN -Controller HOST:PORT [-TailscaleKey KEY]
#
# Or download and run: powershell -ExecutionPolicy Bypass -File .\install-windows.ps1 -Token ...
param(
	[Parameter(Mandatory = $true)][string]$Token,
	[string]$Controller,
	[string]$Relay,
	[string]$RelaySecret,
	[string]$TailscaleKey,
	[string]$Name = $env:COMPUTERNAME,
	[string]$AgentUrl
)
$ErrorActionPreference = "Stop"

# The release workflow rewrites this to the exact repo + tag it publishes from.
$ReleaseBase = "https://github.com/LibreTether/libretether/releases/latest/download"

if ($Relay -and $Controller) { throw "Use -Relay or -Controller, not both." }
if ($Relay) {
	if (-not $RelaySecret) { throw "-Relay requires -RelaySecret." }
} elseif (-not $Controller) {
	throw "Provide -Relay HOST:PORT -RelaySecret SECRET, or -Controller HOST:PORT."
}

$BinDir = Join-Path $env:LOCALAPPDATA "LibreTether"
$Bin = Join-Path $BinDir "libretether-agent.exe"
Write-Host "==> LibreTether agent install for $Name"

# 1. Tailscale (direct mode with a pre-auth key only).
if ($TailscaleKey) {
	if (-not $Controller) { throw "-TailscaleKey only applies with -Controller." }
	if (-not (Get-Command tailscale -ErrorAction SilentlyContinue)) {
		throw "Install Tailscale from https://tailscale.com/download/windows, then re-run."
	}
	tailscale up --reset --authkey $TailscaleKey
}

# 2. Download the agent (override with $env:LIBRETETHER_AGENT_BIN / _URL or -AgentUrl).
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
if ($env:LIBRETETHER_AGENT_BIN) {
	Copy-Item $env:LIBRETETHER_AGENT_BIN $Bin -Force
} else {
	$Url = if ($AgentUrl) { $AgentUrl } elseif ($env:LIBRETETHER_AGENT_URL) { $env:LIBRETETHER_AGENT_URL } else { "$ReleaseBase/libretether-agent-windows-x86_64.exe" }
	Write-Host "==> Downloading agent from $Url"
	Invoke-WebRequest -Uri $Url -OutFile $Bin
}

# 3. Enroll and register the logon task.
if ($Relay) {
	& $Bin enroll --relay $Relay --relay-secret $RelaySecret --token $Token
} else {
	& $Bin enroll --controller $Controller --token $Token
}
& $Bin install

Write-Host "==> Done. $Name is now reachable from your LibreTether controller."
