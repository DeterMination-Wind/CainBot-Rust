param(
    [string]$NapCatDir = '',
    [string]$TargetDir = '',
    [string]$OwnerUserId = '',
    [string]$AiBaseUrl = '',
    [string]$AiApiKey = '',
    [string]$DatabaseRoot = '',
    [string]$AccountUin = '',
    [switch]$NoPrompt,
    [switch]$StartAfterInstall
)

$ErrorActionPreference = 'Stop'

function Write-Step([string]$Message) {
    Write-Host "[CainBot Installer] $Message"
}

function Test-NapCatDir([string]$PathValue) {
    if ([string]::IsNullOrWhiteSpace($PathValue)) {
        return $false
    }
    $launcherPath = Join-Path $PathValue 'launcher.bat'
    return (Test-Path $launcherPath)
}

function Get-NapCatCandidates {
    $results = New-Object System.Collections.Generic.List[string]
    $push = {
        param([string]$Candidate)
        if ([string]::IsNullOrWhiteSpace($Candidate)) {
            return
        }
        try {
            $resolved = [System.IO.Path]::GetFullPath($Candidate)
        } catch {
            return
        }
        if ((Test-NapCatDir $resolved) -and -not $results.Contains($resolved)) {
            $results.Add($resolved)
        }
    }

    & $push (Join-Path $PSScriptRoot 'NapCat.Shell')
    & $push (Join-Path $PSScriptRoot '..\NapCat.Shell')
    & $push (Join-Path $HOME 'Documents\NapCat.Shell')
    & $push (Join-Path $HOME 'Desktop\NapCat.Shell')
    & $push (Join-Path $HOME 'Downloads\NapCat.Shell')
    & $push 'C:\NapCat.Shell'

    $searchRoots = @(
        (Join-Path $HOME 'Documents'),
        (Join-Path $HOME 'Desktop'),
        (Join-Path $HOME 'Downloads')
    ) | Select-Object -Unique

    foreach ($root in $searchRoots) {
        if (-not (Test-Path $root)) {
            continue
        }
        Get-ChildItem -Path $root -Directory -Recurse -Depth 3 -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '^NapCat(\.Shell)?$' } |
            ForEach-Object { & $push $_.FullName }
    }

    return @($results)
}

function Prompt-Value([string]$Label, [string]$Default = '', [switch]$Required) {
    if ($NoPrompt) {
        if ($Required -and [string]::IsNullOrWhiteSpace($Default)) {
            throw "缺少必填参数：$Label"
        }
        return $Default
    }

    while ($true) {
        $promptText = if ([string]::IsNullOrWhiteSpace($Default)) {
            $Label
        } else {
            "$Label [$Default]"
        }
        $value = Read-Host $promptText
        if ([string]::IsNullOrWhiteSpace($value)) {
            $value = $Default
        }
        if (-not $Required -or -not [string]::IsNullOrWhiteSpace($value)) {
            return $value
        }
        Write-Host "该项不能为空，请重新输入。"
    }
}

function Select-AccountUin([string]$ShellDir, [string]$PreferredValue) {
    if (-not [string]::IsNullOrWhiteSpace($PreferredValue)) {
        return $PreferredValue.Trim()
    }
    $configDir = Join-Path $ShellDir 'config'
    $candidates = @(Get-ChildItem -Path $configDir -Filter 'onebot11_*.json' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -eq 1) {
        return ($candidates[0].BaseName -replace '^onebot11_', '').Trim()
    }
    if ($candidates.Count -gt 1 -and -not $NoPrompt) {
        Write-Host "检测到多个 NapCat 账号配置："
        for ($i = 0; $i -lt $candidates.Count; $i++) {
            $uin = ($candidates[$i].BaseName -replace '^onebot11_', '').Trim()
            Write-Host ("  {0}. {1}" -f ($i + 1), $uin)
        }
        while ($true) {
            $selected = Read-Host "请选择要绑定的 QQ 号序号"
            $index = 0
            if ([int]::TryParse($selected, [ref]$index) -and $index -ge 1 -and $index -le $candidates.Count) {
                return ($candidates[$index - 1].BaseName -replace '^onebot11_', '').Trim()
            }
            Write-Host "输入无效，请重新选择。"
        }
    }
    return (Prompt-Value 'NapCat 登录 QQ 号(AccountUin)' '' -Required)
}

function New-RandomToken {
    $bytes = New-Object byte[] 24
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($bytes)
    return ([Convert]::ToBase64String($bytes)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

function Ensure-JsonProperty([object]$Object, [string]$Name, $Value) {
    if ($Object.PSObject.Properties.Name -contains $Name) {
        $Object.$Name = $Value
    } else {
        $Object | Add-Member -MemberType NoteProperty -Name $Name -Value $Value
    }
}

function Ensure-Directory([string]$PathValue) {
    if (-not (Test-Path $PathValue)) {
        New-Item -ItemType Directory -Force -Path $PathValue | Out-Null
    }
}

function Update-NapCatHttpSseConfig([string]$ShellDir, [string]$Uin) {
    $configPath = Join-Path $ShellDir ("config\onebot11_{0}.json" -f $Uin)
    if (-not (Test-Path $configPath)) {
        throw "未找到 NapCat 配置文件：$configPath"
    }

    $json = Get-Content -Raw -Encoding UTF8 $configPath | ConvertFrom-Json
    if (-not $json.network) {
        Ensure-JsonProperty $json 'network' ([pscustomobject]@{})
    }
    if (-not ($json.network.PSObject.Properties.Name -contains 'httpSseServers')) {
        Ensure-JsonProperty $json.network 'httpSseServers' @()
    }

    $servers = @($json.network.httpSseServers)
    $existing = $servers | Where-Object { $_.name -eq 'httpSseServer' } | Select-Object -First 1
    $token = if ($existing -and -not [string]::IsNullOrWhiteSpace($existing.token)) {
        [string]$existing.token
    } else {
        New-RandomToken
    }

    $server = [pscustomobject]@{
        name              = 'httpSseServer'
        enable            = $true
        host              = '127.0.0.1'
        port              = 3000
        enableCors        = $true
        enableWebsocket   = $false
        messagePostFormat = 'array'
        token             = $token
        debug             = $false
        reportSelfMessage = $false
    }

    $updated = @()
    $replaced = $false
    foreach ($item in $servers) {
        if (-not $replaced -and $item.name -eq 'httpSseServer') {
            $updated += $server
            $replaced = $true
        } else {
            $updated += $item
        }
    }
    if (-not $replaced) {
        $updated += $server
    }

    $json.network.httpSseServers = @($updated)
    $json | ConvertTo-Json -Depth 100 | Set-Content -Encoding UTF8 $configPath

    return @{
        ConfigPath = $configPath
        Token = $token
        BaseUrl = 'http://127.0.0.1:3000'
    }
}

function Load-OrCreateConfig([string]$TargetConfigPath, [string]$TemplatePath) {
    $sourcePath = if (Test-Path $TargetConfigPath) { $TargetConfigPath } else { $TemplatePath }
    return Get-Content -Raw -Encoding UTF8 $sourcePath | ConvertFrom-Json
}

function Save-Config([object]$Config, [string]$PathValue) {
    $Config | ConvertTo-Json -Depth 100 | Set-Content -Encoding UTF8 $PathValue
}

function Write-TextFile([string]$PathValue, [string]$Content) {
    Set-Content -Path $PathValue -Value $Content -Encoding UTF8
}

$packageRoot = $PSScriptRoot
$payloadRoot = Join-Path $packageRoot 'payload'
if (-not (Test-Path (Join-Path $payloadRoot 'cainbot-rs.exe'))) {
    throw "未找到发布负载目录：$payloadRoot"
}

Write-Step '开始安装 CainBot。'

$candidateNapCatDirs = Get-NapCatCandidates
$detectedNapCatDir = if (Test-NapCatDir $NapCatDir) {
    [System.IO.Path]::GetFullPath($NapCatDir)
} elseif ($candidateNapCatDirs.Count -gt 0) {
    $candidateNapCatDirs[0]
} else {
    ''
}
$NapCatDir = Prompt-Value 'NapCat.Shell 目录' $detectedNapCatDir -Required
if (-not (Test-NapCatDir $NapCatDir)) {
    throw "NapCat 目录无效，缺少 launcher.bat：$NapCatDir"
}
$NapCatDir = [System.IO.Path]::GetFullPath($NapCatDir)

$defaultTargetDir = if ([string]::IsNullOrWhiteSpace($TargetDir)) {
    Join-Path (Split-Path $NapCatDir -Parent) 'CainBot'
} else {
    $TargetDir
}
$TargetDir = Prompt-Value 'CainBot 安装目录' $defaultTargetDir -Required
$TargetDir = [System.IO.Path]::GetFullPath($TargetDir)
Ensure-Directory $TargetDir

$AccountUin = Select-AccountUin $NapCatDir $AccountUin
$napcatInfo = Update-NapCatHttpSseConfig $NapCatDir $AccountUin

$databaseCandidates = @(
    $DatabaseRoot,
    (Join-Path (Split-Path $NapCatDir -Parent) 'codex'),
    (Join-Path $HOME 'Documents\codex'),
    (Join-Path $HOME 'codex')
) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
$defaultDatabaseRoot = ($databaseCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1)
if ([string]::IsNullOrWhiteSpace($defaultDatabaseRoot)) {
    $defaultDatabaseRoot = Join-Path (Split-Path $NapCatDir -Parent) 'codex'
}
$DatabaseRoot = Prompt-Value '数据库目录(databaseRoot)' $defaultDatabaseRoot -Required
$DatabaseRoot = [System.IO.Path]::GetFullPath($DatabaseRoot)

$OwnerUserId = Prompt-Value 'CainBot 主人 QQ 号(ownerUserId)' ($OwnerUserId.Trim()) -Required
$AiBaseUrl = Prompt-Value 'AI Base URL' ($(if ($AiBaseUrl.Trim()) { $AiBaseUrl.Trim() } else { 'http://127.0.0.1:15721/v1' })) -Required
$AiApiKey = Prompt-Value 'AI API Key' ($AiApiKey.Trim()) -Required

Write-Step '复制程序文件。'
robocopy $payloadRoot $TargetDir /E /R:1 /W:1 /NFL /NDL /NJH /NJS /NP | Out-Null
$robocopyExitCode = $LASTEXITCODE
if ($robocopyExitCode -ge 8) {
    throw "复制发布文件失败，robocopy 退出码：$robocopyExitCode"
}

Ensure-Directory (Join-Path $TargetDir 'data')
Ensure-Directory (Join-Path $TargetDir 'data\logs')
Ensure-Directory (Join-Path $TargetDir 'data\Knowledge')
Ensure-Directory (Join-Path $TargetDir 'data\release-downloads')

$templateConfigPath = Join-Path $TargetDir 'config.example.json'
$targetConfigPath = Join-Path $TargetDir 'config.json'
$config = Load-OrCreateConfig $targetConfigPath $templateConfigPath

if (-not $config.paths) { Ensure-JsonProperty $config 'paths' ([pscustomobject]@{}) }
if (-not $config.napcat) { Ensure-JsonProperty $config 'napcat' ([pscustomobject]@{}) }
if (-not $config.bot) { Ensure-JsonProperty $config 'bot' ([pscustomobject]@{}) }
if (-not $config.ai) { Ensure-JsonProperty $config 'ai' ([pscustomobject]@{}) }
if (-not $config.issueRepair) { Ensure-JsonProperty $config 'issueRepair' ([pscustomobject]@{}) }
if (-not $config.qa) { Ensure-JsonProperty $config 'qa' ([pscustomobject]@{}) }
if (-not $config.qa.answer) { Ensure-JsonProperty $config.qa 'answer' ([pscustomobject]@{}) }

Ensure-JsonProperty $config.paths 'databaseRoot' $DatabaseRoot
Ensure-JsonProperty $config.issueRepair 'databaseRoot' $DatabaseRoot
Ensure-JsonProperty $config.qa.answer 'databaseRoot' $DatabaseRoot
Ensure-JsonProperty $config.issueRepair 'codexRoot' $DatabaseRoot
Ensure-JsonProperty $config.qa.answer 'codexRoot' $DatabaseRoot
Ensure-JsonProperty $config.qa.answer 'localBuildRoot' (Join-Path $DatabaseRoot 'builds')
Ensure-JsonProperty $config.qa.answer 'vanillaRepoRoot' (Join-Path $DatabaseRoot 'Mindustry-master')
Ensure-JsonProperty $config.qa.answer 'xRepoRoot' (Join-Path $DatabaseRoot 'MindustryX-main')
Ensure-JsonProperty $config.napcat 'baseUrl' $napcatInfo.BaseUrl
Ensure-JsonProperty $config.napcat 'eventBaseUrl' $napcatInfo.BaseUrl
Ensure-JsonProperty $config.napcat 'webUiConfigPath' (Join-Path $NapCatDir 'config\webui.json')
if (-not $config.napcat.headers) { Ensure-JsonProperty $config.napcat 'headers' ([pscustomobject]@{}) }
Ensure-JsonProperty $config.napcat.headers 'Authorization' ("Bearer {0}" -f $napcatInfo.Token)
Ensure-JsonProperty $config.bot 'ownerUserId' $OwnerUserId
Ensure-JsonProperty $config.ai 'baseUrl' $AiBaseUrl
Ensure-JsonProperty $config.ai 'apiKey' $AiApiKey
Ensure-JsonProperty $config.qa.answer 'rag' ([pscustomobject]@{
    enabled = $true
    autoInject = $true
    timeoutMs = 2500
    maxResults = 6
    maxPathResults = 4
    maxContentResults = 6
    maxFileSizeBytes = 1048576
    maxPromptChars = 4200
    roots = @([pscustomobject]@{
        alias = 'codex'
        path = $DatabaseRoot
    })
})
Save-Config $config $targetConfigPath

$runCainBotBat = @"
@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
if not exist "config.json" (
  echo [ERROR] Missing config.json
  pause
  exit /b 1
)
echo [INFO] Starting CainBot Rust...
"@ + "`r`n" + '".\cainbot-rs.exe"' + "`r`n" + @"
echo.
echo [INFO] CainBot exited with code: %errorlevel%
pause
"@
Write-TextFile (Join-Path $TargetDir 'run-cain-bot.bat') $runCainBotBat

$escapedNapCatDir = $NapCatDir.Replace('"', '""')
$runCainServiceBat = @"
@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"
set "NAPCAT_DIR=$escapedNapCatDir"
if not exist "%NAPCAT_DIR%\launcher.bat" (
  echo [ERROR] Missing NapCat launcher: %NAPCAT_DIR%\launcher.bat
  pause
  exit /b 1
)
echo [INFO] Starting NapCat.Shell...
start "NapCat.Shell" /D "%NAPCAT_DIR%" cmd /c "call \"%NAPCAT_DIR%\launcher.bat\" -q $AccountUin"
echo [INFO] Waiting for NapCat to initialize...
timeout /t 8 /nobreak >nul
call "%~dp0run-cain-bot.bat"
"@
Write-TextFile (Join-Path $TargetDir 'run-cain-service.bat') $runCainServiceBat

Ensure-Directory (Join-Path $TargetDir 'scripts')
$autostartCmd = @"
@echo off
chcp 65001 >nul
setlocal
cd /d "$($TargetDir.Replace('"', '""'))"
call "$($TargetDir.Replace('"', '""'))\run-cain-service.bat"
"@
Write-TextFile (Join-Path $TargetDir 'scripts\autostart-run-cain-service.cmd') $autostartCmd

$summaryText = @"
CainBot 已安装完成。

安装目录: $TargetDir
NapCat 目录: $NapCatDir
QQ 号: $AccountUin
数据库目录: $DatabaseRoot
NapCat HTTP/SSE 配置: $($napcatInfo.ConfigPath)

后续可直接双击：
1. run-cain-bot.bat
2. run-cain-service.bat
"@
Write-TextFile (Join-Path $TargetDir 'INSTALL_RESULT.txt') $summaryText

Write-Step '安装完成。'
Write-Host $summaryText

$shouldStart = $StartAfterInstall.IsPresent
if (-not $NoPrompt -and -not $shouldStart) {
    $answer = Read-Host '是否立即启动 CainBot 服务？(y/N)'
    if ($answer -match '^(y|yes)$') {
        $shouldStart = $true
    }
}

if ($shouldStart) {
    Write-Step '正在启动 CainBot 服务。'
    Start-Process -FilePath (Join-Path $TargetDir 'run-cain-service.bat') -WorkingDirectory $TargetDir
}


