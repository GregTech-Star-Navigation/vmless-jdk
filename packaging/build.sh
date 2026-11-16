#!/usr/bin/env bash
# bash 里调 packaging/build.ps1 —— 参数原样透传。
#
#   packaging/build.sh                    # 全量：cargo build + zip + 安装包 + 校验和 + release notes
#   packaging/build.sh -SkipBuild         # 只重打包（用 target/release 下现成的 exe）
#   packaging/build.sh -SkipBuild -SkipInstaller
#   packaging/build.sh -Makensis /c/tools/nsis/makensis.exe
#
# 需要 PowerShell（Windows 自带）与 cargo；NSIS 只在打安装包时需要。
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
PS1="$HERE/build.ps1"
[ -f "$PS1" ] || { echo "找不到 $PS1" >&2; exit 2; }

# Windows PowerShell 5.1 按 ANSI 读 .ps1：文件没有 UTF-8 BOM 时中文注释会把语法读坏
# （报一堆 TerminatorExpectedAtEndOfString）。编辑器把 BOM 吃掉时在这里补回来。
if [ "$(head -c 3 "$PS1" | od -An -tx1 | tr -d ' \n')" != "efbbbf" ]; then
    printf '\xEF\xBB\xBF' | cat - "$PS1" > "$PS1.bom" && mv "$PS1.bom" "$PS1"
    echo "（已给 build.ps1 补上 UTF-8 BOM）"
fi

if command -v pwsh >/dev/null 2>&1; then
    SHELL_EXE=pwsh
elif command -v powershell >/dev/null 2>&1; then
    SHELL_EXE=powershell
else
    echo "需要 PowerShell（pwsh 或 powershell）" >&2
    exit 2
fi

exec "$SHELL_EXE" -NoProfile -ExecutionPolicy Bypass -File "$(command -v cygpath >/dev/null 2>&1 && cygpath -m "$PS1" || printf '%s' "$PS1")" "$@"
