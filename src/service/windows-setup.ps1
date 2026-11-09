$ErrorActionPreference = 'Stop'

function Assert-RegularPath($Path) {
    $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($item -and ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
        throw "The NodeStats service path cannot be a reparse point: $Path"
    }
}

function Set-Access($Path, [string[]] $Grants) {
    & "$env:SystemRoot\System32\icacls.exe" $Path /inheritance:r /grant:r @Grants | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "Could not set permissions on $Path." }
}

try {
    $root = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) 'NodeStats Agent'
    $agent = Join-Path $root 'nodestats.exe'
    $state = Join-Path $root 'state'
    $identity = Join-Path $state 'identity.json'
    $lock = Join-Path $state 'agent.lock'
    $access = Join-Path $state 'cloudflare-access.json'
    foreach ($path in @($root, $agent, $state, $identity, $lock, $access)) {
        Assert-RegularPath $path
    }

    $oldTask = Get-ScheduledTask -TaskName NodeStats -ErrorAction SilentlyContinue
    if ($oldTask -and $oldTask.State -eq 'Running') {
        Stop-ScheduledTask -TaskName NodeStats
    }
    New-Item -ItemType Directory -Path $root, $state -Force | Out-Null

    # The service can read its credential and open its lock. Administrators own the
    # directory and executable. Other local accounts have no access to the credential.
    $admins = '*S-1-5-32-544'
    $system = '*S-1-5-18'
    $service = '*S-1-5-19'
    Set-Access $root @("${admins}:(OI)(CI)F", "${system}:(OI)(CI)F", "${service}:(OI)(CI)RX")
    Set-Access $state @("${admins}:(OI)(CI)F", "${system}:(OI)(CI)F", "${service}:(OI)(CI)RX")
    Copy-Item -LiteralPath $sourceBinary -Destination $agent -Force
    Copy-Item -LiteralPath $sourceIdentity -Destination $identity -Force
    if (Test-Path -LiteralPath $sourceAccess) { Copy-Item -LiteralPath $sourceAccess -Destination $access -Force }
    elseif (Test-Path -LiteralPath $access) { Remove-Item -LiteralPath $access }
    if (-not (Test-Path -LiteralPath $lock)) { New-Item -ItemType File -Path $lock | Out-Null }
    Set-Access $lock @("${admins}:F", "${system}:F", "${service}:M")

    $account = [Security.Principal.SecurityIdentifier]::new('S-1-5-19').Translate([Security.Principal.NTAccount]).Value
    $action = New-ScheduledTaskAction -Execute $agent -Argument ('run --managed --state-dir "' + $state + '"')
    $trigger = New-ScheduledTaskTrigger -AtStartup
    $principal = New-ScheduledTaskPrincipal -UserId $account -LogonType ServiceAccount -RunLevel Limited
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval ([TimeSpan]::FromMinutes(1))
    Register-ScheduledTask -TaskName NodeStats -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Force | Out-Null
    Start-ScheduledTask -TaskName NodeStats
} catch {
    # UAC may start this process in a separate window. Show the failure before that window closes.
    Add-Type -AssemblyName System.Windows.Forms
    [void][Windows.Forms.MessageBox]::Show($_.Exception.Message, 'NodeStats setup')
    exit 1
}
