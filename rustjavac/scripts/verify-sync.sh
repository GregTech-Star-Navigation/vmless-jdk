#!/usr/bin/env bash
# 跨线程 synchronized 的回归：三个入口 × 两个样例，**带超时与内存上限**。
#
# 为什么单独有这个脚本：2026-11-17 出过一次「JIT 编的方法回调解释器，但解释器线程
# 没登记」的卡死 —— 表现是工作集按 ~90MB/s 涨到 18.8GB、进程永不退出，把机器吃爆。
# 这类 bug 不能用「跑一遍看输出对不对」来发现，必须**同时**断言峰值内存和墙钟时间。
#
# 用法：./scripts/verify-sync.sh [bin目录] [内存上限MB] [超时秒]
#   默认 bin 目录 target/release（或 $BIN），上限 200MB、超时 30 秒
set -u
BIN="${1:-${BIN:-target/release}}"
MAXMB="${2:-200}"
SECS="${3:-30}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
SRC="$ROOT/samples/thread"
JAVAC="${JAVAC:-C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/javac.exe}"
JAVA="${JAVA:-C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe}"
export VMLESS_JDK="${VMLESS_JDK:-D:/vmless/patch/img}"

# 带外文件监视器会锁住「被执行过的 exe」，所以一律复制到新目录再跑（覆盖会静默失败）
RUNBIN="/d/vmless/.syncbin-$$"
OUT="$(mktemp -d 2>/dev/null || echo /tmp/verify-sync-$$)"
mkdir -p "$OUT"
trap 'rm -rf "$RUNBIN" "$OUT" 2>/dev/null' EXIT
mkdir -p "$RUNBIN"
for e in rustjava run_jit run_jdk compile_src; do
    cp "$BIN/$e.exe" "$RUNBIN/" 2>/dev/null || cp "$BIN/examples/$e.exe" "$RUNBIN/" 2>/dev/null
done
for e in rustjava run_jit run_jdk; do
    [ -x "$RUNBIN/$e.exe" ] || { echo "缺 $e.exe（bin 目录：$BIN）" >&2; exit 2; }
done

# 样例：用 javac 现编（这两份源码在 samples/thread 下，是本回归的一部分）
# 注意：这个环境里 MSYS_NO_PATHCONV=1，打给 javac 的路径必须先转成 Windows 形式
"$JAVAC" -d "$(cygpath -w "$OUT")" "$(cygpath -w "$SRC/SyncOne.java")" "$(cygpath -w "$SRC/SyncFour.java")" >/dev/null 2>&1 || {
    echo "javac 编译样例失败（设 JAVAC 指向可用的 javac）" >&2; exit 2; }

MEMRUN="$(cygpath -w "$HERE/memrun.ps1")"
ok=0; bad=0
# run <样例名> <入口> <参数> <期望出现的字符串...>
run() {
    local name="$1" entry="$2" arg="$3"; shift 3
    local o="$OUT/$name-$entry.out" e="$OUT/$name-$entry.err"
    local line
    line=$(powershell -NoProfile -ExecutionPolicy Bypass -File "$MEMRUN" \
        -Exe "$(cygpath -w "$RUNBIN/$entry.exe")" -Arg "$arg" \
        -Secs "$SECS" -MaxMB "$MAXMB" -Out "$(cygpath -w "$o")" -ErrFile "$(cygpath -w "$e")" 2>/dev/null | tr -d '\r')
    local res; res=$(printf '%s\n' "$line" | grep -m1 '^RESULT' )
    local status maxmb secs
    status=$(printf '%s' "$res" | sed -n 's/.*status=\([a-z]*\).*/\1/p')
    maxmb=$(printf '%s' "$res" | sed -n 's/.*maxMB=\([0-9]*\).*/\1/p')
    secs=$(printf '%s' "$res" | sed -n 's/.*secs=\([0-9.]*\).*/\1/p')
    local why=""
    [ "$status" = "ok" ] || why="status=$status"
    if [ -n "$why" ]; then :; else
        for want in "$@"; do
            grep -qF -- "$want" "$o" || why="缺输出 '$want'"
        done
    fi
    printf '%-10s %-8s  峰值%4sMB  %5ss  ' "$name" "$entry" "$maxmb" "$secs"
    if [ -z "$why" ]; then
        echo "✓"; ok=$((ok + 1))
    else
        echo "✗ $why"; bad=$((bad + 1))
        sed -n '1,3p' "$e" | sed 's/^/    stderr: /'
    fi
}

echo "上限：${MAXMB}MB / ${SECS}s    三个入口都跑（输出 + 内存 + 墙钟）"
echo "-------------------------------------------------------------------"
for entry in rustjava run_jit; do
    run SyncOne "$entry" "\"$(cygpath -w "$OUT/SyncOne.class")\"" "count=1000"
    run SyncFour "$entry" "\"$(cygpath -w "$OUT/SyncFour.class")\"" "count=400000" "true"
done
# run_jdk 的参数是「类目录 主类名」
run SyncOne run_jdk "$(cygpath -w "$OUT") SyncOne" "count=1000"
run SyncFour run_jdk "$(cygpath -w "$OUT") SyncFour" "count=400000" "true"
echo "-------------------------------------------------------------------"
echo "通过 $ok 项，失败 $bad 项"
[ "$bad" -eq 0 ]
