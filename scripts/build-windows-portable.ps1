param(
    [string]$Version = '',
    [string]$OutputRoot = 'dist'
)

$ErrorActionPreference = 'Stop'

function Write-Step([string]$Message) {
    Write-Host "[build-windows-portable] $Message"
}

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = (Get-Content -Raw package.json | ConvertFrom-Json).version
}
$normalizedVersion = $Version.Trim()
if ($normalizedVersion.StartsWith('v')) {
    $normalizedVersion = $normalizedVersion.Substring(1)
}
if ([string]::IsNullOrWhiteSpace($normalizedVersion)) {
    throw 'Version 不能为空。'
}
$versionLabel = if ($normalizedVersion -match '^[0-9]') {
    "v$normalizedVersion"
} else {
    $normalizedVersion
}

$outputRootPath = Join-Path $repoRoot $OutputRoot
$stageRoot = Join-Path $outputRootPath ("CainBot-{0}-windows-portable" -f $versionLabel)
$payloadRoot = Join-Path $stageRoot 'payload'
$zipPath = Join-Path $outputRootPath ("CainBot-{0}-windows-portable.zip" -f $versionLabel)
$hashPath = "$zipPath.sha256"

Write-Step "构建 Rust release 二进制 v$normalizedVersion"
cargo build --release

Write-Step '准备发布目录'
if (Test-Path $stageRoot) {
    Remove-Item -Recurse -Force $stageRoot
}
if (Test-Path $zipPath) {
    Remove-Item -Force $zipPath
}
if (Test-Path $hashPath) {
    Remove-Item -Force $hashPath
}
New-Item -ItemType Directory -Force -Path $payloadRoot | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $payloadRoot 'data\logs') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $payloadRoot 'data\Knowledge') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $payloadRoot 'data\release-downloads') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $payloadRoot 'scripts') | Out-Null

Write-Step '复制程序与资源'
Copy-Item -Force (Join-Path $repoRoot 'target\release\cainbot-rs.exe') (Join-Path $payloadRoot 'cainbot-rs.exe')
Copy-Item -Force (Join-Path $repoRoot 'config.example.json') (Join-Path $payloadRoot 'config.example.json')
Copy-Item -Force (Join-Path $repoRoot 'README.md') (Join-Path $payloadRoot 'README.md')
Copy-Item -Force (Join-Path $repoRoot 'LICENSE') (Join-Path $payloadRoot 'LICENSE')
Copy-Item -Force (Join-Path $repoRoot 'scripts\enable-napcat-http-sse.ps1') (Join-Path $payloadRoot 'scripts\enable-napcat-http-sse.ps1')
Copy-Item -Recurse -Force (Join-Path $repoRoot 'prompts') (Join-Path $payloadRoot 'prompts')
Copy-Item -Force (Join-Path $repoRoot 'release\windows\install-cainbot.bat') (Join-Path $stageRoot 'install-cainbot.bat')
Copy-Item -Force (Join-Path $repoRoot 'release\windows\install-cainbot.ps1') (Join-Path $stageRoot 'install-cainbot.ps1')

$readmeText = @"
CainBot v$normalizedVersion Windows 便携安装包

使用方法：
1. 解压整个压缩包。
2. 双击 install-cainbot.bat。
3. 按提示自动寻找 NapCat.Shell、写入配置并生成启动脚本。
4. 安装完成后，进入目标目录运行 run-cain-service.bat。

本包内已包含 Rust 可执行文件，不依赖本地 Node.js。
"@
Set-Content -Encoding UTF8 -Path (Join-Path $stageRoot 'START_HERE.txt') -Value $readmeText

Write-Step '压缩发布包'
Compress-Archive -Path (Join-Path $stageRoot '*') -DestinationPath $zipPath -CompressionLevel Optimal

$hash = (Get-FileHash -Algorithm SHA256 $zipPath).Hash.ToLowerInvariant()
Set-Content -Encoding UTF8 -Path $hashPath -Value ("{0} *{1}" -f $hash, [System.IO.Path]::GetFileName($zipPath))

Write-Step "发布包已生成：$zipPath"
Write-Step "校验文件已生成：$hashPath"

if ($env:GITHUB_OUTPUT) {
    Add-Content -Path $env:GITHUB_OUTPUT -Value ("zip_path={0}" -f $zipPath)
    Add-Content -Path $env:GITHUB_OUTPUT -Value ("sha256_path={0}" -f $hashPath)
}



