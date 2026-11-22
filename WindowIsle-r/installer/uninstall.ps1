﻿$ErrorActionPreference='Stop'
$installDir=[System.IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\WindowIsle'))
if([System.IO.Path]::GetFullPath($PSScriptRoot) -ne $installDir){throw '卸载脚本只能在窗屿安装目录中运行。'}
$exePath=Join-Path $installDir 'windowisle.exe'
Get-Process windowisle -ErrorAction SilentlyContinue | Where-Object {$_.Path -eq $exePath} | ForEach-Object {
    $null=$_.CloseMainWindow();if(-not $_.WaitForExit(3000)){Stop-Process -Id $_.Id}
}
# 内存清理服务（windowisle.exe 的副本，装在 Program Files、以 SYSTEM 运行）：卸载需要一次管理员确认。
if((Get-Service -Name WindowIsleClean -ErrorAction SilentlyContinue) -and (Test-Path -LiteralPath $exePath)){
    try{Start-Process -FilePath $exePath -ArgumentList '--service-uninstall' -Verb RunAs -WindowStyle Hidden -Wait}catch{Write-Warning '未卸载内存清理服务（已取消管理员确认），可稍后在管理员终端运行：sc.exe delete WindowIsleClean'}
}
$shell=New-Object -ComObject WScript.Shell
$links=@((Join-Path $shell.SpecialFolders.Item('Desktop') '窗屿.lnk'),(Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\窗屿.lnk'),(Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\窗屿设置.lnk'))
foreach($path in $links){if((Test-Path -LiteralPath $path) -and $shell.CreateShortcut($path).TargetPath -eq $exePath){Remove-Item -LiteralPath $path}}
$runKey='HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Remove-ItemProperty -Path $runKey -Name WindowIsle -ErrorAction SilentlyContinue
Remove-Item -LiteralPath 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\WindowIsle' -ErrorAction SilentlyContinue
foreach($name in @('windowisle.exe','windowisle-previous.exe','windowisle-v0.2-backup.exe','THIRD_PARTY_NOTICES.txt','LICENSE','uninstall.ps1')){
    $path=Join-Path $installDir $name;if(Test-Path -LiteralPath $path){Remove-Item -LiteralPath $path}
}
if(-not(Get-ChildItem -LiteralPath $installDir -Force)){Remove-Item -LiteralPath $installDir}
Write-Output '窗屿已卸载。待办、饮水记录和设置仍保存在本机用户数据目录中。'
