<#
.SYNOPSIS
    打 vmless 的 Windows 安装包：portable zip + NSIS 安装包 + Release 清单。

.DESCRIPTION
    版本号从 rustjavac/Cargo.toml 的 [package] version 读，不手写。
    产物落在 -OutDir（默认 <仓库>/dist）：
        vmless-<ver>-win-x64.zip
        vmless-<ver>-setup.exe
        SHA256SUMS
        release-notes.md

.PARAMETER SkipBuild
    跳过 cargo build，只拿 target/release 下已有的可执行文件重打包。

.PARAMETER SkipInstaller
    不生成 NSIS 安装包（没装 NSIS 的环境用）。

.PARAMETER Version
    覆盖版本号（默认读 Cargo.toml）。只用于临时验证，正式发布不要传。

.PARAMETER Makensis
    makensis.exe 路径。默认依次查 PATH、NSIS_HOME、Program Files。

.PARAMETER OutDir
    产物目录，默认 <仓库>/dist。

.EXAMPLE
    pwsh -File packaging/build.ps1
    pwsh -File packaging/build.ps1 -SkipBuild -SkipInstaller
#>
[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$SkipInstaller,
    [string]$Version,
    [string]$Makensis,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$RustDir = Join-Path $RepoRoot 'rustjavac'
$CargoToml = Join-Path $RustDir 'Cargo.toml'
$Changelog = Join-Path $RepoRoot 'CHANGELOG.md'
if (-not $OutDir) { $OutDir = Join-Path $RepoRoot 'dist' }
$OutDir = [System.IO.Path]::GetFullPath($OutDir)

function Write-Step([string]$msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok([string]$msg) { Write-Host "    $msg" -ForegroundColor Green }
function Write-Warn2([string]$msg) { Write-Host "    $msg" -ForegroundColor Yellow }

function Get-Utf8([string]$path) {
    return [System.IO.File]::ReadAllText($path, [System.Text.Encoding]::UTF8)
}

# ---------------------------------------------------------------- 版本号
Write-Step "读取版本号"
if (-not (Test-Path $CargoToml)) { throw "找不到 $CargoToml" }
$toml = Get-Utf8 $CargoToml
$m = [regex]::Match($toml, '(?m)^\s*version\s*=\s*"([^"]+)"')
if (-not $m.Success) { throw "$CargoToml 里没有 version 字段" }
$CargoVersion = $m.Groups[1].Value
if (-not $Version) { $Version = $CargoVersion }
Write-Ok "crate 版本 = $CargoVersion$(if ($Version -ne $CargoVersion) { "（打包用 $Version，来自 -Version）" } else { '' })"

# CHANGELOG 最新一节的版本，用来发现「Cargo.toml 忘了跟」的情况
$latestChangelog = $null
if (Test-Path $Changelog) {
    $heads = [regex]::Matches((Get-Utf8 $Changelog), '(?m)^##\s+(\S+)')
    if ($heads.Count -gt 0) { $latestChangelog = $heads[$heads.Count - 1].Groups[1].Value }
}
if ($latestChangelog -and $latestChangelog -ne $Version) {
    Write-Warn2 "注意：Cargo.toml 是 $Version，CHANGELOG 最新一节是 $latestChangelog —— 按 Cargo.toml 打包"
}

# ---------------------------------------------------------------- 构建
$BinDir = Join-Path $RustDir 'target\release'
$ExamplesDir = Join-Path $BinDir 'examples'
if (-not $SkipBuild) {
    Write-Step "cargo build --release（rustjava）"
    Push-Location $RustDir
    try {
        & cargo build --release --bin rustjava
        if ($LASTEXITCODE -ne 0) { throw "cargo build --bin rustjava 失败（exit $LASTEXITCODE）" }
        Write-Step "cargo build --release（examples: compile_src / run_jit）"
        & cargo build --release --example compile_src --example run_jit
        if ($LASTEXITCODE -ne 0) { throw "cargo build --example 失败（exit $LASTEXITCODE）" }
    }
    finally { Pop-Location }
} else {
    Write-Step "跳过 cargo build（-SkipBuild）"
}

$exes = @(
    @{ Name = 'rustjava.exe'; Path = Join-Path $BinDir 'rustjava.exe' },
    @{ Name = 'compile_src.exe'; Path = Join-Path $ExamplesDir 'compile_src.exe' },
    @{ Name = 'run_jit.exe'; Path = Join-Path $ExamplesDir 'run_jit.exe' }
)

# ---------------------------------------------------------------- JDK 工具集
# 工具的真身分布在 6 个 crate 里。**不能只打 jdk-tools**：除它自己的 15 个之外，
# 其余 17 个名字的实现都在兄弟 crate 里 —— 它们在 jdk-tools/src/bin 下曾经是
# 「打印一句『本 crate 不提供该工具』然后 exit 1」的桩，把真身整个挡住了（桩已删除）。
#
# 这里按 (crate, bin) 精确列出，**不按目录 glob**：残留的旧 exe（比如删桩之前的
# 桩产物）会躺在 target/release 里被悄悄打进包，那种包"能装上、跑起来却是一句退出码 1"，
# 是最难查的一类事故。
$ToolMap = @(
    @{ Crate = 'jdk-tools';  Bins = @('jimage', 'jmod', 'jlink', 'javadoc', 'jdb', 'jshell', 'jrunscript', 'jwebserver', 'jpackage', 'jdeprscan', 'jnativescan', 'serialver', 'rmiregistry', 'javaw', 'jps', 'jar') },
    @{ Crate = 'jdk-attach'; Bins = @('jcmd', 'jinfo', 'jmap', 'jstack', 'jstat', 'jstatd') },
    @{ Crate = 'jdk-crypto'; Bins = @('jarsigner', 'keytool', 'ktab') },
    @{ Crate = 'jdk-krb';    Bins = @('kinit', 'klist') },
    @{ Crate = 'jdk-jfr';    Bins = @('jfr', 'jhsdb') },
    @{ Crate = 'jdk-gui';    Bins = @('jabswitch', 'jaccessinspector', 'jaccesswalker', 'jconsole') }
)

# 工具集用**自己的** target 根目录（`<repo>/target-tools/<crate>`），不蹭各 crate 的 `target/`：
# 那些目录里可能躺着别的活儿留下的旧 exe（删桩之前的桩产物就那样躺着过），而 cargo 不会清理
# 孤儿产物；更麻烦的是带外文件监视器会锁住刚跑过的 exe，导致 LNK1104 让打包莫名失败。
# 独立目录 + 每次按 $ToolMap 精确取，两个问题一起消掉。
$ToolTargetRoot = Join-Path $RepoRoot 'target-tools'

if (-not $SkipBuild) {
    foreach ($c in $ToolMap) {
        Write-Step "cargo build --release（$($c.Crate)：$($c.Bins.Count) 个工具）"
        $prevTarget = $env:CARGO_TARGET_DIR
        # 每个 crate 用自己的子目录：这几个 crate 不是同一个 workspace，共用一个目录
        # 只会让并行构建互相等锁，还容易和别的活儿撞名。
        $env:CARGO_TARGET_DIR = Join-Path $ToolTargetRoot $c.Crate
        Push-Location (Join-Path $RepoRoot $c.Crate)
        try {
            & cargo build --release --bins
            if ($LASTEXITCODE -ne 0) { throw "cargo build --bins（$($c.Crate)）失败（exit $LASTEXITCODE）" }
        }
        finally {
            Pop-Location
            $env:CARGO_TARGET_DIR = $prevTarget
        }
    }
}

foreach ($c in $ToolMap) {
    foreach ($b in $c.Bins) {
        $exes += @{ Name = "$b.exe"; Path = (Join-Path $ToolTargetRoot ("{0}\release\{1}.exe" -f $c.Crate, $b)) }
    }
}

foreach ($e in $exes) {
    if (-not (Test-Path $e.Path)) { throw "缺少 $($e.Path) —— 去掉 -SkipBuild 重新编译" }
}

# ---------------------------------------------------------------- 暂存目录
Write-Step "装配暂存目录"
# 先清掉同版本的旧产物：否则「这次没生成的安装包」会留在 dist 里，却不在 SHA256SUMS 中
if (Test-Path $OutDir) {
    Get-ChildItem -Path $OutDir -Filter "vmless-$Version-*" -File -ErrorAction SilentlyContinue |
        ForEach-Object { Write-Host "    移除旧产物 $($_.Name)"; Remove-Item $_.FullName -Force }
}
$stage = Join-Path $OutDir 'stage'
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Path (Join-Path $stage 'bin') -Force | Out-Null
foreach ($e in $exes) { Copy-Item $e.Path (Join-Path $stage ('bin\' + $e.Name)) -Force }

Copy-Item (Join-Path $PSScriptRoot 'README.txt') (Join-Path $stage 'README.txt') -Force
$license = Join-Path $RepoRoot 'LICENSE'
if (Test-Path $license) {
    Copy-Item $license (Join-Path $stage 'LICENSE') -Force
} else {
    Write-Warn2 "仓库根没有 LICENSE —— 打包里放占位说明（发布前务必补）"
    Copy-Item (Join-Path $PSScriptRoot 'LICENSE.placeholder') (Join-Path $stage 'LICENSE') -Force
}

$exeInfo = foreach ($e in $exes) {
    $item = Get-Item (Join-Path $stage ('bin\' + $e.Name))
    [pscustomobject]@{ Name = $e.Name; Bytes = $item.Length }
}

# ---------------------------------------------------------------- portable zip
Write-Step "生成 portable zip"
$zipName = "vmless-$Version-win-x64.zip"
$zipPath = Join-Path $OutDir $zipName
if (Test-Path $zipPath) { Remove-Item $zipPath -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath -CompressionLevel Optimal
Write-Ok "$zipName  $([math]::Round((Get-Item $zipPath).Length / 1MB, 2)) MB"

# ---------------------------------------------------------------- NSIS
$setupName = "vmless-$Version-setup.exe"
$setupPath = Join-Path $OutDir $setupName
if ($SkipInstaller) {
    Write-Step "跳过 NSIS 安装包（-SkipInstaller）"
} else {
    Write-Step "查找 makensis"
    $makensisExe = $Makensis
    if (-not $makensisExe) {
        $cand = @()
        $onPath = Get-Command makensis.exe -ErrorAction SilentlyContinue
        if ($onPath) { $cand += $onPath.Source }
        if ($env:NSIS_HOME) { $cand += (Join-Path $env:NSIS_HOME 'makensis.exe') }
        $cand += ('{0}\NSIS\makensis.exe' -f ${env:ProgramFiles(x86)})
        $cand += ('{0}\NSIS\makensis.exe' -f $env:ProgramFiles)
        $cand += (Join-Path $PSScriptRoot 'tools\nsis\makensis.exe')
        foreach ($c in $cand) { if ($c -and (Test-Path $c)) { $makensisExe = $c; break } }
    }
    if ($makensisExe -and (Test-Path $makensisExe)) {
        Write-Ok "makensis = $makensisExe"
        if (Test-Path $setupPath) { Remove-Item $setupPath -Force }
        & $makensisExe /INPUTCHARSET UTF8 /V2 `
            "/DSTAGE=$stage" `
            "/DVERSION=$Version" `
            "/DOUTFILE=$setupPath" `
            (Join-Path $PSScriptRoot 'vmless.nsi')
        if ($LASTEXITCODE -ne 0) { throw "makensis 失败（exit $LASTEXITCODE）" }
        Write-Ok "$setupName  $([math]::Round((Get-Item $setupPath).Length / 1MB, 2)) MB"
    } else {
        Write-Warn2 "没找到 makensis.exe —— 跳过安装包（装 NSIS 3.x 或用 -Makensis 指定）"
        Write-Warn2 "  下载：https://nsis.sourceforge.io/Download （解压即用，不需要安装）"
        $setupPath = $null
    }
}

# ---------------------------------------------------------------- 校验和
Write-Step "生成 SHA256SUMS"
$sumsPath = Join-Path $OutDir 'SHA256SUMS'
$lines = @()
foreach ($p in @($zipPath, $setupPath)) {
    if (-not $p -or -not (Test-Path $p)) { continue }
    $h = (Get-FileHash -Path $p -Algorithm SHA256).Hash.ToLowerInvariant()
    $lines += ('{0}  {1}' -f $h, (Split-Path -Leaf $p))
}
[System.IO.File]::WriteAllText($sumsPath, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))
$lines | ForEach-Object { Write-Ok $_ }

# ---------------------------------------------------------------- release notes
Write-Step "生成 release-notes.md"
$notesPath = Join-Path $OutDir 'release-notes.md'
$notes = New-Object System.Text.StringBuilder
[void]$notes.AppendLine("# vmless $Version")
[void]$notes.AppendLine()
[void]$notes.AppendLine("Windows x64 便携包：``vmless-$Version-win-x64.zip``（解压即用，无需安装）。")
[void]$notes.AppendLine("安装包：``vmless-$Version-setup.exe``（装到 ``%LOCALAPPDATA%\vmless``，可加入用户 PATH，带卸载）。")
[void]$notes.AppendLine()
[void]$notes.AppendLine('校验：`SHA256SUMS` 与产物同目录，`certutil -hashfile <文件> SHA256` 可核对。')
[void]$notes.AppendLine()
if (Test-Path $Changelog) {
    $cl = Get-Utf8 $Changelog
    $secs = [regex]::Matches($cl, '(?ms)^(##\s+(\S+).*?)(?=^##\s|\z)')
    if ($secs.Count -gt 0) {
        $newest = $secs[$secs.Count - 1]
        # 只有「版本号 == 最新一节」时才直接用该节：Cargo.toml 落后于 CHANGELOG 时，
        # 精确匹配到的会是历史上同号的旧小节（0.1.0 就是最早那节），拿它当发布说明是错的。
        $picked = $null
        if ($Version -eq $newest.Groups[2].Value) {
            $picked = $newest
        } else {
            $picked = $newest
            [void]$notes.AppendLine("> 说明：``rustjavac/Cargo.toml`` 的版本是 ``$Version``，CHANGELOG 最新一节是 ``$($newest.Groups[2].Value)``（版本号还没跟上）。下面取最新一节。")
            [void]$notes.AppendLine()
        }
        [void]$notes.AppendLine($picked.Groups[1].Value.Trim())
    }
} else {
    [void]$notes.AppendLine('CHANGELOG.md 缺失，本节留空。')
}
[System.IO.File]::WriteAllText($notesPath, $notes.ToString(), (New-Object System.Text.UTF8Encoding($false)))
Write-Ok "release-notes.md  $((Get-Item $notesPath).Length) 字节"

# ---------------------------------------------------------------- 汇总
# 暂存目录只是中间产物：产物都进 dist 根了，清掉免得 dist 里多一层（也避免被 upload-artifact 一起传上去）
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
Write-Step "产物清单"
$rows = @()
foreach ($p in @($zipPath, $setupPath)) {
    if ($p -and (Test-Path $p)) {
        $h = (Get-FileHash -Path $p -Algorithm SHA256).Hash.ToLowerInvariant()
        $rows += [pscustomobject]@{
            File   = Split-Path -Leaf $p
            MB     = [math]::Round((Get-Item $p).Length / 1MB, 3)
            SHA256 = $h
        }
    }
}
$rows += [pscustomobject]@{ File = 'SHA256SUMS'; MB = [math]::Round((Get-Item $sumsPath).Length / 1KB, 1); SHA256 = '' }
$rows += [pscustomobject]@{ File = 'release-notes.md'; MB = [math]::Round((Get-Item $notesPath).Length / 1KB, 1); SHA256 = '' }
$rows | Format-Table -AutoSize | Out-String -Width 200 | Write-Host

Write-Step "可执行文件（暂存区 bin/）"
$exeInfo | Format-Table -AutoSize | Out-String -Width 200 | Write-Host
Write-Ok "输出目录：$OutDir"
