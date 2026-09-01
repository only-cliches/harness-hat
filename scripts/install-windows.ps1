<#
Installs Docker Desktop and Harness Hat for the active Windows console user.
Run this file from PowerShell; it elevates itself when WSL or Docker Desktop
needs administrator permissions.
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repository = 'only-cliches/harness-hat'

function Test-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-ActiveConsoleUser {
    $user = (Get-CimInstance Win32_ComputerSystem).UserName
    if ([string]::IsNullOrWhiteSpace($user)) {
        throw 'No active Windows console user was found.'
    }
    return $user
}

function Resolve-ReleaseVersion {
    param([string]$RequestedVersion)
    if ($RequestedVersion -ne 'latest') {
        if ($RequestedVersion.StartsWith('v')) {
            return $RequestedVersion
        }
        return "v$RequestedVersion"
    }
    $release = Invoke-RestMethod -Headers @{ 'User-Agent' = 'Harness-Hat-Installer' } `
        -Uri "https://api.github.com/repos/$Repository/releases/latest"
    if ([string]::IsNullOrWhiteSpace($release.tag_name)) {
        throw 'Could not resolve the latest Harness Hat release.'
    }
    return $release.tag_name
}

function Ensure-UserPath {
    param([string]$BinDirectory)
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = @()
    if (-not [string]::IsNullOrWhiteSpace($userPath)) {
        $parts = $userPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }
    if ($parts -notcontains $BinDirectory) {
        [Environment]::SetEnvironmentVariable('Path', ($BinDirectory + ';' + ($parts -join ';')), 'User')
    }
    $env:Path = "$BinDirectory;$env:Path"
}

function Invoke-WslSetup {
    if (-not (Get-Command wsl.exe -ErrorAction SilentlyContinue)) {
        throw 'WSL is unavailable on this Windows installation. Enable WSL 2, then rerun this installer.'
    }
    & wsl.exe --install --no-distribution
    if ($LASTEXITCODE -notin 0, 3010) {
        Write-Warning "WSL installation returned exit code $LASTEXITCODE. A reboot may be required."
    }
    & wsl.exe --update
    if ($LASTEXITCODE -notin 0, 3010) {
        Write-Warning "WSL update returned exit code $LASTEXITCODE. A reboot may be required."
    }
    & wsl.exe --set-default-version 2
    if ($LASTEXITCODE -notin 0, 3010) {
        Write-Warning "WSL 2 is not ready yet (exit code $LASTEXITCODE). Docker Desktop may need a reboot before it starts."
    }
}

function Get-DockerCommand {
    $command = Get-Command docker.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    foreach ($candidate in @(
        "$env:LOCALAPPDATA\Docker\resources\bin\docker.exe",
        "$env:ProgramFiles\Docker\Docker\resources\bin\docker.exe"
    )) {
        if (Test-Path $candidate) {
            return $candidate
        }
    }
    return $null
}

function Ensure-Docker {
    $docker = Get-DockerCommand
    if ($docker) {
        try {
            & $docker version | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Host 'Docker is already installed and ready.'
                return
            }
        }
        catch {
            Write-Warning 'Docker is installed but not ready; leaving the existing installation unchanged.'
            return
        }
    }

    Write-Host 'Installing WSL 2 and Docker Desktop...'
    Invoke-WslSetup
    $installer = Join-Path $script:WorkDirectory 'Docker Desktop Installer.exe'
    Invoke-WebRequest -Uri 'https://desktop.docker.com/win/main/amd64/Docker%20Desktop%20Installer.exe' `
        -OutFile $installer
    $process = Start-Process -FilePath $installer -Wait -PassThru -ArgumentList @(
        'install', '--quiet', '--accept-license', '--user', '--backend=wsl-2', '--no-windows-containers'
    )
    if ($process.ExitCode -ne 0) {
        throw "Docker Desktop installation failed with exit code $($process.ExitCode)."
    }

    $desktop = Join-Path $env:LOCALAPPDATA 'Docker\Docker Desktop.exe'
    if (Test-Path $desktop) {
        Start-Process -FilePath $desktop | Out-Null
    }
    for ($attempt = 1; $attempt -le 45; $attempt++) {
        $docker = Get-DockerCommand
        if ($docker) {
            & $docker version | Out-Null
            if ($LASTEXITCODE -eq 0) {
                Write-Host 'Docker Desktop is ready.'
                return
            }
        }
        Start-Sleep -Seconds 2
    }
    Write-Warning 'Docker Desktop was installed but is not ready yet; continue after it finishes starting or after a reboot.'
}

if (-not (Test-Administrator)) {
    if ([string]::IsNullOrWhiteSpace($PSCommandPath)) {
        throw 'Save this installer to a .ps1 file before running it so it can elevate safely.'
    }
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-Version', $Version)
    if ($Force) {
        $arguments += '-Force'
    }
    $elevated = Start-Process -FilePath 'powershell.exe' -Verb RunAs -Wait -PassThru -ArgumentList $arguments
    exit $elevated.ExitCode
}

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
    throw "Unsupported Windows architecture: $env:PROCESSOR_ARCHITECTURE. Harness Hat releases currently support Windows AMD64 only."
}

$activeUser = Get-ActiveConsoleUser
$currentUser = [Security.Principal.WindowsIdentity]::GetCurrent().Name
if (-not $activeUser.Equals($currentUser, [StringComparison]::OrdinalIgnoreCase)) {
    throw "The active console user ($activeUser) differs from the elevated user ($currentUser). Run this installer from the active user's PowerShell session."
}

$script:WorkDirectory = Join-Path ([IO.Path]::GetTempPath()) ("harness-hat-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $script:WorkDirectory | Out-Null
try {
    Ensure-Docker

    $binDirectory = Join-Path $env:LOCALAPPDATA 'HarnessHat\bin'
    $hatExecutable = Join-Path $binDirectory 'hat.exe'
    if ((Test-Path $hatExecutable) -and -not $Force) {
        Write-Host "Harness Hat is already installed at $hatExecutable; use -Force to replace it."
        Ensure-UserPath -BinDirectory $binDirectory
        exit 0
    }
    if ((Test-Path $hatExecutable) -and $Force) {
        & $hatExecutable uninstall
        if ($LASTEXITCODE -ne 0) {
            Write-Warning 'The previous Harness Hat background task could not be removed cleanly; continuing with replacement.'
        }
    }

    $release = Resolve-ReleaseVersion -RequestedVersion $Version
    $archive = Join-Path $script:WorkDirectory 'hat-x86_64-pc-windows-msvc.zip'
    $archiveUrl = "https://github.com/$Repository/releases/download/$release/hat-x86_64-pc-windows-msvc.zip"
    Write-Host "Installing Harness Hat $release..."
    Invoke-WebRequest -Uri $archiveUrl -OutFile $archive
    $extractDirectory = Join-Path $script:WorkDirectory 'harness-hat'
    Expand-Archive -Path $archive -DestinationPath $extractDirectory -Force
    $releasedHat = Join-Path $extractDirectory 'hat.exe'
    $releasedDaemon = Join-Path $extractDirectory 'hat-daemon.exe'
    if (-not (Test-Path $releasedHat) -or -not (Test-Path $releasedDaemon)) {
        throw 'The release archive does not contain both Harness Hat executables.'
    }
    New-Item -ItemType Directory -Force -Path $binDirectory | Out-Null
    Copy-Item -Force $releasedHat $hatExecutable
    Copy-Item -Force $releasedDaemon (Join-Path $binDirectory 'hat-daemon.exe')
    Ensure-UserPath -BinDirectory $binDirectory
    & $hatExecutable install
    if ($LASTEXITCODE -ne 0) {
        throw "Harness Hat background-agent installation failed with exit code $LASTEXITCODE."
    }
    Write-Host "Harness Hat installation complete for $activeUser."
}
finally {
    Remove-Item -Recurse -Force $script:WorkDirectory -ErrorAction SilentlyContinue
}
