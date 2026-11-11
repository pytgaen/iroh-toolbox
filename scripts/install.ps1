<#
.SYNOPSIS
Install iroh-toolbox from a GitHub release on Windows.

.DESCRIPTION
Downloads the Windows archive of a release, checks it against the release's
SHA-256 manifest, installs iroh-toolbox.exe and adds its directory to the user
PATH.

  irm https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.ps1 | iex
  & ([scriptblock]::Create((irm .../install.ps1))) -Version v0.1.0 -NoModifyPath

.PARAMETER Version
Release to install, for example v0.1.0. Default: the latest release.

.PARAMETER BinDir
Installation directory. Default: $env:IROH_TOOLBOX_INSTALL_DIR, or
$env:LOCALAPPDATA\Programs\iroh-toolbox.

.PARAMETER NoModifyPath
Do not add the installation directory to the user PATH.
#>
param(
    [string]$Version = '',
    [string]$BinDir = '',
    [switch]$NoModifyPath
)

function Install-IrohToolbox {
    param([string]$Version, [string]$BinDir, [bool]$NoModifyPath)
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2 by default.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $releases = if ($env:IROH_TOOLBOX_INSTALL_BASE_URL) { $env:IROH_TOOLBOX_INSTALL_BASE_URL } else { 'https://github.com/pytgaen/iroh-toolbox/releases' }
    $sourceBuild = 'build from source instead: https://github.com/pytgaen/iroh-toolbox#build-from-source'
    # HTTPS only, except plain HTTP to a loopback test server.
    $base = $null
    if (-not [Uri]::TryCreate($releases, [UriKind]::Absolute, [ref]$base) -or -not (
            $base.Scheme -eq 'https' -or ($base.Scheme -eq 'http' -and $base.IsLoopback -and -not $base.UserInfo))) {
        throw 'IROH_TOOLBOX_INSTALL_BASE_URL must use https (http is accepted only for 127.0.0.1 or localhost)'
    }

    $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { $note = $null }
        # Windows 11 on ARM runs x64 executables through emulation.
        'ARM64' { $note = 'no native ARM64 archive; installing the x64 build, which Windows 11 runs through emulation' }
        default { throw "no release archive for Windows $arch; $sourceBuild" }
    }
    $target = 'x86_64-pc-windows-msvc'

    $latest = ''
    if (-not $Version) {
        # The latest release redirects to its tag page.
        $request = [Net.HttpWebRequest]::Create("$releases/latest")
        $request.Method = 'HEAD'
        try {
            $response = $request.GetResponse()
        } catch {
            throw "no published release found at $releases; $sourceBuild"
        }
        $url = $response.ResponseUri.AbsoluteUri
        $response.Close()
        if ($url -match '/tag/(v[^/?#]+)$') {
            $Version = $Matches[1]
        } else {
            throw "no published release found at $releases; $sourceBuild"
        }
        $latest = ' (latest)'
    }
    if (-not $Version.StartsWith('v')) { $Version = "v$Version" }
    if ($Version -notmatch '^v[0-9][A-Za-z0-9.-]*$') {
        throw "invalid version: $Version (expected vX.Y.Z)"
    }

    $archive = "iroh-toolbox-$Version-$target.zip"
    $manifest = "iroh-toolbox-$Version-sha256sums.txt"
    $temp = Join-Path ([IO.Path]::GetTempPath()) ("iroh-toolbox-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        Write-Host 'iroh-toolbox installer'
        Write-Host "  version    $Version$latest"
        Write-Host "  platform   $target"
        if ($note) { Write-Host "note: $note" }
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$Version/$manifest" -OutFile (Join-Path $temp $manifest)
        } catch {
            throw "release $Version not found at $releases"
        }
        $expected = $null
        foreach ($line in Get-Content (Join-Path $temp $manifest)) {
            $fields = $line -split '\s+', 2
            if ($fields.Count -eq 2 -and $fields[1].TrimStart('*') -eq $archive) {
                $expected = $fields[0]
                break
            }
        }
        if (-not $expected) { throw "release $Version has no archive for $target" }
        $archivePath = Join-Path $temp $archive
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$releases/download/$Version/$archive" -OutFile $archivePath
        } catch {
            throw "cannot download $archive"
        }
        if ((Get-FileHash -Algorithm SHA256 $archivePath).Hash -ne $expected) {
            throw "checksum mismatch for $archive; nothing was installed"
        }
        Write-Host "  checksum   verified ($manifest)"

        $extract = Join-Path $temp 'extract'
        Expand-Archive -Path $archivePath -DestinationPath $extract
        $staged = Join-Path $extract 'iroh-toolbox.exe'
        if (-not (Test-Path -LiteralPath $staged -PathType Leaf)) {
            throw "cannot extract iroh-toolbox.exe from $archive"
        }

        if (-not $BinDir) {
            $BinDir = if ($env:IROH_TOOLBOX_INSTALL_DIR) { $env:IROH_TOOLBOX_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\iroh-toolbox' }
        }
        $BinDir = (New-Item -ItemType Directory -Force -Path $BinDir).FullName
        $destination = Join-Path $BinDir 'iroh-toolbox.exe'
        $previous = $null
        if (Test-Path -LiteralPath $destination -PathType Leaf) {
            try { $previous = (& $destination -V 2>$null) -join ' ' } catch { $previous = $null }
        }
        try {
            Copy-Item -LiteralPath $staged -Destination $destination -Force
        } catch {
            throw "cannot write $destination; close running iroh-toolbox processes or choose another -BinDir"
        }
        if ($previous) { Write-Host "  replaced   $previous" }
        Write-Host "  installed  $destination"

        $inPath = ($env:Path -split ';') -contains $BinDir
        if (-not $NoModifyPath) {
            # Edit the registry value directly to keep %VARIABLE% references expandable.
            $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
            try {
                $userPath = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                $entries = @($userPath -split ';' | Where-Object { $_ })
                if ($entries -notcontains $BinDir) {
                    $key.SetValue('Path', (($entries + $BinDir) -join ';'), [Microsoft.Win32.RegistryValueKind]::ExpandString)
                    Send-EnvironmentChange
                    Write-Host '  PATH       added to your user PATH; open a new terminal'
                }
            } finally {
                $key.Close()
            }
            if (-not $inPath) { $env:Path = "$env:Path;$BinDir" }
        } elseif (-not $inPath) {
            Write-Host "note: $BinDir is not on PATH; add it to your user PATH or omit -NoModifyPath"
        }
        & $destination -V
        if ($LASTEXITCODE -ne 0) { throw 'the installed executable does not run on this machine' }
    } finally {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Tells running programs, such as Explorer, that the user environment changed.
function Send-EnvironmentChange {
    if (-not ('IrohToolbox.Environment' -as [type])) {
        Add-Type -Namespace IrohToolbox -Name Environment -MemberDefinition @'
[DllImport("user32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr SendMessageTimeout(IntPtr hWnd, uint Msg, UIntPtr wParam, string lParam, uint fuFlags, uint uTimeout, out UIntPtr lpdwResult);
'@
    }
    $result = [UIntPtr]::Zero
    # HWND_BROADCAST, WM_SETTINGCHANGE, SMTO_ABORTIFHUNG, 5 seconds.
    [void][IrohToolbox.Environment]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
}

Install-IrohToolbox -Version $Version -BinDir $BinDir -NoModifyPath $NoModifyPath.IsPresent
