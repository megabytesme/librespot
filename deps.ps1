$SdkRoot = "C:\Program Files (x86)\Windows Kits\10\Lib"
$SdkVersion = "10.0.10240.0"

$ArmUm = "$SdkRoot\$SdkVersion\um\arm"
$ArmUcrt = "$SdkRoot\$SdkVersion\ucrt\arm"

if (!(Test-Path $ArmUm) -or !(Test-Path $ArmUcrt)) {
    Write-Error "Required UWP SDK not installed: $SdkVersion"
    exit 1
}

Write-Host "Using Windows SDK $SdkVersion" -ForegroundColor Cyan

$Env:LIB = "$ArmUm;$ArmUcrt;$Env:LIB"
Write-Host "LIB = $Env:LIB" -ForegroundColor Green

$HackDir = Join-Path (Get-Location) "lib_hack"
New-Item -ItemType Directory -Force -Path $HackDir | Out-Null

Copy-Item "$ArmUm\mincore.lib" "$HackDir\windows.0.52.0.lib" -Force
Copy-Item "$ArmUm\mincore.lib" "$HackDir\windows.0.53.0.lib" -Force

Write-Host "Alias libs created in $HackDir" -ForegroundColor Green

$Env:LIB = "$PWD\lib_hack;$Env:LIB"