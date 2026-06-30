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
	[string]$Token,
	[string]$Controller,
	[string]$Relay,
	[string]$RelaySecret,
	[string]$TailscaleKey,
	[string]$ControllerKey,
	[switch]$Pair,
	[string]$Code,
	[string]$Name = $env:COMPUTERNAME,
	[string]$AgentUrl,
	[switch]$NoRdp
)
$ErrorActionPreference = "Stop"

# The release workflow rewrites this to the exact repo + tag it publishes from.
$ReleaseBase = "https://github.com/LibreTether/libretether/releases/latest/download"

# GET a URL with a few retries on transient failures (5xx, timeout, connection),
# but NOT on a genuine 404 — that rethrows immediately so the caller can tell "the
# server hiccuped" from "this asset doesn't exist". Windows PowerShell 5.1 has no
# -MaximumRetryCount, hence the manual loop.
function Invoke-WebWithRetry {
	param([string]$Uri, [string]$OutFile)
	for ($attempt = 1; $attempt -le 3; $attempt++) {
		try {
			if ($OutFile) { return Invoke-WebRequest -Uri $Uri -OutFile $OutFile -UseBasicParsing }
			return Invoke-WebRequest -Uri $Uri -UseBasicParsing
		} catch {
			$status = if ($_.Exception.Response) { [int]$_.Exception.Response.StatusCode } else { 0 }
			if ($status -eq 404 -or $attempt -ge 3) { throw }
			Start-Sleep -Seconds 2
		}
	}
}

# Verify a downloaded file against its published <url>.sha256 sidecar. A transient
# fetch failure is retried (above) so a network blip isn't mistaken for an absent
# checksum. The official release always ships a sidecar, so a 404 (or a persistent
# failure) there is a hard failure (-Required — fail closed rather than install an
# unverified agent). A custom -AgentUrl may legitimately have none; for that
# (-Required:$false) we warn and continue.
function Test-Checksum {
	param([string]$Url, [string]$File, [bool]$Required)
	# Download the sidecar to a temp file and read it back, rather than reading the
	# response's .Content. GitHub serves every release asset (the .sha256 included)
	# as application/octet-stream, and Invoke-WebRequest -UseBasicParsing yields an
	# empty .Content string for a non-text content type — so reading .Content makes a
	# present checksum look missing and aborts the install. -OutFile writes the raw
	# bytes regardless of content type, exactly like the agent download above.
	$sumFile = "$File.sha256"
	try {
		Invoke-WebWithRetry -Uri "$Url.sha256" -OutFile $sumFile | Out-Null
		$expected = (Get-Content -Path $sumFile -Raw).Trim()
	} catch {
		$expected = $null
	} finally {
		Remove-Item $sumFile -ErrorAction SilentlyContinue
	}
	if ([string]::IsNullOrWhiteSpace($expected)) {
		if ($Required) {
			Remove-Item $File -ErrorAction SilentlyContinue
			throw "Couldn't fetch a checksum for $Url - refusing to install an unverified agent."
		}
		Write-Host "==> No published checksum for $Url (custom URL) - skipping integrity check."
		return
	}
	$actual = (Get-FileHash -Algorithm SHA256 -Path $File).Hash
	if ($expected.ToLower() -ne $actual.ToLower()) {
		Remove-Item $File -ErrorAction SilentlyContinue
		throw "Checksum mismatch for the downloaded agent (expected $expected, got $actual)."
	}
	Write-Host "==> Verified agent checksum."
}

# Pairing mode (a short code from the browser portal) needs only the relay; the
# token, secret and controller key all arrive over the PAKE channel. Otherwise it's
# classic enrollment with a token.
if ($Code) {
	if (-not $Relay) { throw "-Code requires -Relay HOST:PORT." }
} else {
	if (-not $Token) { throw "Provide -Token (or use -Pair -Code for a portal code)." }
	if ($Relay -and $Controller) { throw "Use -Relay or -Controller, not both." }
	if ($Relay) {
		if (-not $RelaySecret) { throw "-Relay requires -RelaySecret." }
	} elseif (-not $Controller) {
		throw "Provide -Relay HOST:PORT -RelaySecret SECRET, or -Controller HOST:PORT."
	}
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
	# A custom URL may lack a checksum sidecar; the official release always has one,
	# so require it there (fail closed) and only warn-and-skip for a custom URL.
	$custom = [bool]($AgentUrl -or $env:LIBRETETHER_AGENT_URL)
	$Url = if ($AgentUrl) { $AgentUrl } elseif ($env:LIBRETETHER_AGENT_URL) { $env:LIBRETETHER_AGENT_URL } else { "$ReleaseBase/libretether-agent-windows-x86_64.exe" }
	Write-Host "==> Downloading agent from $Url"
	Invoke-WebWithRetry -Uri $Url -OutFile $Bin | Out-Null
	Test-Checksum -Url $Url -File $Bin -Required (-not $custom)
}

# Run an agent subcommand and fail loudly. The agent is a GUI-subsystem binary
# (no console window for the background service), which PowerShell's call operator
# would NOT wait for — so use Start-Process -Wait and check the exit code. We also
# redirect stdout/stderr to files: a GUI-subsystem process discards its output
# otherwise, so this is the only way to actually see *why* a step failed (the error
# the agent printed), rather than just a bare exit code.
function Invoke-Agent {
	param([string[]]$AgentArgs)
	$outFile = [System.IO.Path]::GetTempFileName()
	$errFile = [System.IO.Path]::GetTempFileName()
	try {
		$p = Start-Process -FilePath $Bin -ArgumentList $AgentArgs -NoNewWindow -Wait -PassThru `
			-RedirectStandardOutput $outFile -RedirectStandardError $errFile
		$out = (Get-Content -Raw $outFile -ErrorAction SilentlyContinue)
		$err = (Get-Content -Raw $errFile -ErrorAction SilentlyContinue)
		if ($out) { Write-Host $out.TrimEnd() }
		if ($p.ExitCode -ne 0) {
			$detail = (@($err, $out) | Where-Object { $_ -and $_.Trim() } | ForEach-Object { $_.Trim() }) -join "`n"
			if (-not $detail) { $detail = "(the agent produced no output)" }
			throw "agent '$($AgentArgs[0])' failed (exit $($p.ExitCode)):`n$detail`n`nFull log: $env:APPDATA\libretether-agent\agent.log"
		}
	} finally {
		Remove-Item $outFile, $errFile -Force -ErrorAction SilentlyContinue
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

# 3. Pair (portal code) or enroll (token), then register the logon task. In pair
#    mode the controller key + token arrive over the PAKE channel; in enroll mode the
#    controller key (when supplied) pins the controller identity.
if ($Code) {
	Invoke-Agent @('pair', '--relay', $Relay, '--code', $Code)
} else {
	$KeyArgs = if ($ControllerKey) { @('--controller-key', $ControllerKey) } else { @() }
	if ($Relay) {
		Invoke-Agent (@('enroll', '--relay', $Relay, '--relay-secret', $RelaySecret, '--token', $Token) + $KeyArgs)
	} else {
		Invoke-Agent (@('enroll', '--controller', $Controller, '--token', $Token) + $KeyArgs)
	}
}
Invoke-Agent @('install')

# 4. Enable RDP (unless opted out).
if (-not $NoRdp) { Enable-RemoteDesktop }

Write-Host "==> Done. $Name is now reachable from your LibreTether controller."
