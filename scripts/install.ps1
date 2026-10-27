<#
.SYNOPSIS
Installs SimpleTickets on Windows: binaries, optional addons, and a per-user
scheduled task that starts the daemon at logon.

.DESCRIPTION
The daemon runs as a scheduled task of the current user, not as a Windows service.
It starts when you log on and is restarted if it exits. Run it with -DryRun first
to see the plan; nothing is changed in that mode.

.PARAMETER Prefix       Install binaries into <Prefix>\bin (default: %LOCALAPPDATA%\SimpleTickets).
.PARAMETER TicketsDir   Tickets folder the daemon manages (default: ~\SimpleTickets).
.PARAMETER Port         Daemon port on 127.0.0.1 (default: 7878).
.PARAMETER Addons       Comma-separated addons to install, or "none" (default: ask).
.PARAMETER FromSource   Build with cargo from this checkout.
.PARAMETER FromRelease  Download the latest release archive.
.PARAMETER NoService    Do not register the scheduled task.
.PARAMETER NoStart      Register the task but do not start it now.
.PARAMETER Yes          Accept defaults without asking.
.PARAMETER DryRun       Print what would happen, change nothing.
#>
[CmdletBinding()]
param(
    [string]$Prefix = (Join-Path $env:LOCALAPPDATA 'SimpleTickets'),
    [string]$TicketsDir = (Join-Path $HOME 'SimpleTickets'),
    [int]$Port = 7878,
    [string]$Addons = '',
    [switch]$FromSource,
    [switch]$FromRelease,
    [switch]$NoService,
    [switch]$NoStart,
    [switch]$Yes,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

$Repo = 'MichalVacek/SimpleTickets'
$TaskName = 'SimpleTickets'

# Addons: Name, Crate, Binary, Description. Add an entry to offer another addon.
$AddonList = @(
    @{ Name = 'mcp'; Crate = 'simpletickets-mcp'; Binary = 'simpletickets-mcp'; Description = 'MCP server so a client can list, create and update tickets' }
    @{ Name = 'ui'; Crate = 'simpletickets-ui'; Binary = 'simpletickets-ui'; Description = 'Kanban board in the browser, served on 127.0.0.1' }
    @{ Name = 'sync'; Crate = 'simpletickets-sync'; Binary = 'simpletickets-sync'; Description = 'Synchronization to other trackers through connector plugins' }
    @{ Name = 'connector-archive'; Crate = 'simpletickets-connector-archive'; Binary = 'simpletickets-connector-archive'; Description = 'Sync connector writing one file per ticket to a folder' }
    @{ Name = 'connector-jira'; Crate = 'simpletickets-connector-jira'; Binary = 'simpletickets-connector-jira'; Description = 'Sync connector for Jira Cloud (unverified against a live site)' }
)

function Confirm-Default([string]$Prompt, [bool]$Default) {
    if ($Yes -or $DryRun -or -not [Environment]::UserInteractive) { return $Default }
    $hint = if ($Default) { 'Y/n' } else { 'y/N' }
    $reply = Read-Host "$Prompt [$hint]"
    if ($reply -eq '') { return $Default }
    return $reply -match '^[yY]'
}

function Select-Addons {
    if ($Addons -ne '' -and $Addons -ne 'none') {
        foreach ($requested in $Addons.Split(',')) {
            if (-not ($AddonList | Where-Object { $_.Name -eq $requested })) {
                throw "unknown addon '$requested'"
            }
        }
    }
    $selected = @()
    foreach ($addon in $AddonList) {
        if ($Addons -ne '') {
            $chosen = ",$Addons," -like "*,$($addon.Name),*"
        } else {
            $chosen = Confirm-Default "Install addon '$($addon.Name)' ($($addon.Description))?" $true
        }
        if ($chosen) { $selected += $addon }
    }
    return $selected
}

function Get-ReleaseAsset {
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64') { throw "no release archive for CPU $arch (only x86_64 is published)" }
    return 'simpletickets-x86_64-pc-windows-msvc.zip'
}

function Install-Binaries([string[]]$Names, $Selected, [string]$Mode, [string]$RepoRoot, [string]$BinDir) {
    if ($Mode -eq 'source') {
        $cargoArgs = @('build', '--release', '--manifest-path', (Join-Path $RepoRoot 'Cargo.toml'), '-p', 'simpletickets')
        foreach ($addon in $Selected) { $cargoArgs += @('-p', $addon.Crate) }
        if ($DryRun) {
            Write-Host "would run: cargo $($cargoArgs -join ' ')"
            foreach ($name in $Names) { Write-Host "would copy target\release\$name.exe to $BinDir" }
            return
        }
        & cargo @cargoArgs
        if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
        foreach ($name in $Names) {
            Copy-Item (Join-Path $RepoRoot "target\release\$name.exe") (Join-Path $BinDir "$name.exe") -Force
        }
        return
    }

    $asset = Get-ReleaseAsset
    $base = "https://github.com/$Repo/releases/latest/download"
    if ($DryRun) {
        Write-Host "would download $base/$asset, verify it against $base/SHA256SUMS and install: $($Names -join ' ')"
        return
    }
    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("simpletickets-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        try {
            Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)
            Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
        } catch {
            throw "cannot download from $base (no release published yet? try -FromSource): $_"
        }
        $line = Get-Content (Join-Path $tmp 'SHA256SUMS') |
            Where-Object { ($_ -split '\s+', 2)[1] -replace '^\*', '' -eq $asset } | Select-Object -First 1
        if (-not $line) { throw "$asset is not listed in SHA256SUMS" }
        $expected = ($line -split '\s+')[0]
        $actual = (Get-FileHash (Join-Path $tmp $asset) -Algorithm SHA256).Hash
        if ($expected -ne $actual) { throw "checksum mismatch for $asset" }
        Expand-Archive (Join-Path $tmp $asset) -DestinationPath $tmp -Force
        foreach ($name in $Names) {
            $file = Join-Path $tmp "$name.exe"
            if (-not (Test-Path $file)) { throw "$name.exe missing from $asset" }
            Copy-Item $file (Join-Path $BinDir "$name.exe") -Force
        }
    } finally {
        Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Quote-Single([string]$Text) { return "'" + $Text.Replace("'", "''") + "'" }

if ($Port -lt 1 -or $Port -gt 65535) { throw '-Port must be between 1 and 65535' }
if ($FromSource -and $FromRelease) { throw 'use only one of -FromSource and -FromRelease' }

$RepoRoot = Split-Path -Parent $PSScriptRoot
$BinDir = Join-Path $Prefix 'bin'

if ($FromSource) { $Mode = 'source' }
elseif ($FromRelease) { $Mode = 'release' }
elseif ((Test-Path (Join-Path $RepoRoot 'Cargo.toml')) -and (Get-Command cargo -ErrorAction SilentlyContinue)) { $Mode = 'source' }
else { $Mode = 'release' }

$Selected = @(Select-Addons)
$Names = @('simpletickets') + @($Selected | ForEach-Object { $_.Binary })

Write-Host "Install mode: $Mode"
Write-Host "Binaries:     $BinDir ($($Names -join ' '))"
Write-Host "Tickets:      $TicketsDir"
if (-not $NoService) { Write-Host "Service:      scheduled task '$TaskName' at logon (port $Port)" }

if ($DryRun) {
    Write-Host "would create $BinDir and $TicketsDir"
} else {
    New-Item -ItemType Directory -Force -Path $BinDir, $TicketsDir | Out-Null
}
Install-Binaries $Names $Selected $Mode $RepoRoot $BinDir

if (-not $NoService) {
    $exe = Join-Path $BinDir 'simpletickets.exe'
    $command = "& $(Quote-Single $exe) --dir $(Quote-Single $TicketsDir) daemon --port $Port"
    $actionArgs = "-NoProfile -WindowStyle Hidden -Command `"$($command.Replace('"', '\"'))`""
    if ($DryRun) {
        Write-Host "would register scheduled task '$TaskName' for $env:USERNAME:"
        Write-Host "  trigger: at logon"
        Write-Host "  action:  powershell.exe $actionArgs"
        Write-Host "  restart: every 1 minute, up to 3 times, if the daemon exits"
        if (-not $NoStart) { Write-Host "would start the task now" }
    } else {
        $action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument $actionArgs
        $trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) `
            -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -StartWhenAvailable `
            -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings `
            -Description 'SimpleTickets daemon' -Force | Out-Null
        Write-Host "registered scheduled task '$TaskName'"
        if (-not $NoStart -and (Confirm-Default 'Start the daemon now and at every logon?' $true)) {
            Start-ScheduledTask -TaskName $TaskName
            Write-Host 'daemon started'
        } else {
            Write-Host "not started; start it with: Start-ScheduledTask -TaskName $TaskName"
        }
        Write-Host "to remove it: Unregister-ScheduledTask -TaskName $TaskName -Confirm:`$false"
    }
}

if (($env:PATH -split ';') -notcontains $BinDir) { Write-Host "note: $BinDir is not on your PATH" }
if ($Selected | Where-Object { $_.Name -eq 'mcp' }) {
    Write-Host "MCP server: register '$(Join-Path $BinDir 'simpletickets-mcp.exe') --dir $TicketsDir' as a stdio server in your MCP client."
}
if ($Selected | Where-Object { $_.Name -eq 'ui' }) {
    Write-Host "Board: run '$(Join-Path $BinDir 'simpletickets-ui.exe') --dir $TicketsDir' and open the address it prints."
}
if ($Selected | Where-Object { $_.Name -eq 'sync' }) {
    Write-Host "Sync: write $TicketsDir\.simpletickets\sync\config.yml (see docs/sync.md), then run 'simpletickets plugins enable sync'."
}
Write-Host 'done'
