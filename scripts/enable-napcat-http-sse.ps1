param(
    [string]$ShellDir = 'C:\Users\华硕\Documents\NapCat.Shell',
    [string]$AccountUin = '1705087729',
    [string]$ListenHost = '127.0.0.1',
    [int]$Port = 3000,
    [string]$Token = 'CHANGE_ME'
)

$configPath = Join-Path $ShellDir "config\onebot11_${AccountUin}.json"
if (-not (Test-Path $configPath)) {
    throw "未找到配置文件: $configPath"
}

$json = Get-Content -Raw -Encoding UTF8 $configPath | ConvertFrom-Json -Depth 100
if (-not $json.network) {
    $json | Add-Member -MemberType NoteProperty -Name network -Value ([pscustomobject]@{})
}
if (-not ($json.network.PSObject.Properties.Name -contains 'httpSseServers')) {
    $json.network | Add-Member -MemberType NoteProperty -Name httpSseServers -Value @()
}

$server = [pscustomobject]@{
    name              = 'httpSseServer'
    enable            = $true
    host              = $ListenHost
    port              = $Port
    enableCors        = $true
    enableWebsocket   = $false
    messagePostFormat = 'array'
    token             = $Token
    debug             = $false
    reportSelfMessage = $false
}

$servers = @($json.network.httpSseServers)
$index = -1
for ($i = 0; $i -lt $servers.Count; $i++) {
    if ($servers[$i].name -eq 'httpSseServer') {
        $index = $i
        break
    }
}

if ($index -ge 0) {
    $servers[$index] = $server
} else {
    $servers += $server
}

$json.network.httpSseServers = $servers
$json | ConvertTo-Json -Depth 100 | Set-Content -Encoding UTF8 $configPath

Write-Host "已写入 HTTP-SSE 配置: $configPath"
Write-Host "Host : $ListenHost"
Write-Host "Port : $Port"
Write-Host "Token: $Token"
Write-Host "完成后请重启 NapCat Shell。"
