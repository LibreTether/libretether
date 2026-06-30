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
	[string]$ControllerKey,
	[string]$Name = $env:COMPUTERNAME,
	[string]$AgentUrl,
	[switch]$NoRdp
)
$ErrorActionPreference = "Stop"

# The release workflow rewrites this to the exact repo + tag it publishes from.
$ReleaseBase = "https://github.com/LibreTether/libretether/releases/latest/download"

# Verify a downloaded file against its published <url>.sha256 sidecar. A custom
# -AgentUrl may have no sidecar; in that case we warn and continue.
function Test-Checksum {
	param([string]$Url, [string]$File)
	try {
		$expected = ((Invoke-WebRequest -Uri "$Url.sha256" -UseBasicParsing).Content).Trim()
	} catch {
		Write-Host "==> No published checksum for $Url — skipping integrity check."
		return
	}
	$actual = (Get-FileHash -Algorithm SHA256 -Path $File).Hash
	if ($expected.ToLower() -ne $actual.ToLower()) {
		Remove-Item $File -ErrorAction SilentlyContinue
		throw "Checksum mismatch for the downloaded agent (expected $expected, got $actual)."
	}
	Write-Host "==> Verified agent checksum."
}

if ($Relay -and $Controller) { throw "Use -Relay or -Controller, not both." }
if ($Relay) {
	if (-not $RelaySecret) { throw "-Relay requires -RelaySecret." }
} elseif (-not $Controller) {
	throw "Provide -Relay HOST:PORT -RelaySecret SECRET, or -Controller HOST:PORT."
}

$BinDir = Join-Path $env:LOCALAPPDATA "LibreTether"
$Bin = Join-Path $BinDir "libretether-agent.exe"
Write-Host "==> LibreTether agent install for $Name"

# Stop and remove any prior installation so the agent binary can be replaced.
# Unlike Linux, Windows locks a running .exe, so downloading over it fails with
# "being used by another process". End the logon task, kill the process, then
# wait for the OS to release the file handle before we overwrite it.
# (Task name mirrors TASK in libretether-agent/src/service.rs.)
function Remove-ExistingAgent {
	# Best-effort: a fresh machine has no task/process to remove, and schtasks
	# writes "task not found" to stderr — which would otherwise throw under the
	# script-wide $ErrorActionPreference = "Stop". Shadow it locally instead.
	$ErrorActionPreference = "SilentlyContinue"
	schtasks /End /TN "LibreTetherAgent" 2>$null | Out-Null
	schtasks /Delete /TN "LibreTetherAgent" /F 2>$null | Out-Null
	Get-Process -Name "libretether-agent" -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
	# The handle is freed asynchronously after the process exits; wait until the
	# exe opens for exclusive write (or give up after ~5s) before downloading over it.
	if (Test-Path $Bin) {
		foreach ($attempt in 1..50) {
			try { [System.IO.File]::Open($Bin, 'Open', 'ReadWrite', 'None').Close(); break }
			catch { Start-Sleep -Milliseconds 100 }
		}
	}
}

# 1. Tailscale (direct mode with a pre-auth key only).
if ($TailscaleKey) {
	if (-not $Controller) { throw "-TailscaleKey only applies with -Controller." }
	if (-not (Get-Command tailscale -ErrorAction SilentlyContinue)) {
		throw "Install Tailscale from https://tailscale.com/download/windows, then re-run."
	}
	tailscale up --reset --authkey $TailscaleKey
}

# 2. Remove any prior install (so the running .exe isn't locked), then download
#    the agent (override with $env:LIBRETETHER_AGENT_BIN / _URL or -AgentUrl).
Remove-ExistingAgent
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
if ($env:LIBRETETHER_AGENT_BIN) {
	Copy-Item $env:LIBRETETHER_AGENT_BIN $Bin -Force
} else {
	$Url = if ($AgentUrl) { $AgentUrl } elseif ($env:LIBRETETHER_AGENT_URL) { $env:LIBRETETHER_AGENT_URL } else { "$ReleaseBase/libretether-agent-windows-x86_64.exe" }
	Write-Host "==> Downloading agent from $Url"
	Invoke-WebRequest -Uri $Url -OutFile $Bin
	Test-Checksum -Url $Url -File $Bin
}

# Run an agent subcommand and fail loudly. The agent is a GUI-subsystem binary
# (no console window for the background service), which PowerShell's call operator
# would NOT wait for — so use Start-Process -Wait and check the exit code.
function Invoke-Agent {
	param([string[]]$AgentArgs)
	$p = Start-Process -FilePath $Bin -ArgumentList $AgentArgs -NoNewWindow -Wait -PassThru
	if ($p.ExitCode -ne 0) {
		throw "agent '$($AgentArgs[0])' failed (exit $($p.ExitCode)); see $BinDir\agent.log"
	}
}

# Turn on Remote Desktop so the controller's "Connect via RDP" works (otherwise
# the agent's tunnel to 127.0.0.1:3389 is refused). Needs admin and an edition
# with an RDP host (Windows Home has none); best-effort, never blocks the install.
function Enable-RemoteDesktop {
	$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltinRole]::Administrator)
	if (-not $admin) {
		Write-Host "==> Skipped enabling Remote Desktop (not elevated). Re-run as Administrator, or turn on Settings > System > Remote Desktop, to use RDP. Screen control works regardless."
		return
	}
	try {
		Set-ItemProperty -Path 'HKLM:\System\CurrentControlSet\Control\Terminal Server' -Name 'fDenyTSConnections' -Value 0 -ErrorAction Stop
		# Canonical (locale-independent) Remote Desktop firewall group.
		Enable-NetFirewallRule -Group '@FirewallAPI.dll,-28752' -ErrorAction SilentlyContinue
		Write-Host "==> Remote Desktop enabled."
	} catch {
		Write-Host "==> Could not enable Remote Desktop automatically ($_). Turn it on in Settings > System > Remote Desktop to use RDP."
	}
}

# 3. Enroll and register the logon task. The controller key (when supplied) pins
#    the controller identity so the agent only accepts that controller.
$KeyArgs = if ($ControllerKey) { @('--controller-key', $ControllerKey) } else { @() }
if ($Relay) {
	Invoke-Agent (@('enroll', '--relay', $Relay, '--relay-secret', $RelaySecret, '--token', $Token) + $KeyArgs)
} else {
	Invoke-Agent (@('enroll', '--controller', $Controller, '--token', $Token) + $KeyArgs)
}
Invoke-Agent @('install')

# 4. Enable RDP (unless opted out).
if (-not $NoRdp) { Enable-RemoteDesktop }

Write-Host "==> Done. $Name is now reachable from your LibreTether controller."
