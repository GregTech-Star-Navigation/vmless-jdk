#!/usr/bin/env bash
# vmless vs HotSpot 25 基准 —— 一条命令跑完全部维度。
#
#   bash bench/run.sh                # 跑全部（约 10-15 分钟）
#   bash bench/run.sh --quick        # 每项只跑 2 次，快速冒烟（不出正式报告）
#   bash bench/run.sh --rebuild      # 重新编译基准用的 java 程序
#   bash bench/run.sh --only gc,interp
#
# 输出：bench/REPORT.md（人看）、bench/result.json（机器读）、
#       bench/raw/*.jsonl（每次运行一行）、bench/logs/*.log（子进程输出）。
#
# 环境依赖（都在这台机器上装好了）：
#   - /d/vmless/bparv/{rustjava,run_jit,run_jdk}.exe   0.57.0
#   - C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot
#   - csc.exe（.NET Framework 4）用来编 tools/probe.exe
#   - node（汇总与生成 manifest）
set -u

BENCH=/d/vmless/bench
BENCH_W=D:/vmless/bench
JDK_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot"
JAVA="$JDK_HOME/bin/java.exe"
JAVAC="$JDK_HOME/bin/javac.exe"
BPARV=D:/vmless/bparv
CSC=/c/Windows/Microsoft.NET/Framework64/v4.0.30319/csc.exe
NODE=$(command -v node || true)

QUICK=0
REBUILD=0
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --quick) QUICK=1 ;;
    --rebuild) REBUILD=1 ;;
    --only) shift; ONLY="$1" ;;
    *) echo "未知参数 $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
die() { echo "错误: $*" >&2; exit 1; }

# ---------------------------------------------------------------- 0 环境自检
say "0/6 环境自检"
[ -x "$JAVA" ] || die "找不到 $JAVA"
[ -f "$BPARV/run_jit.exe" ] || die "找不到 $BPARV/run_jit.exe"
[ -n "$NODE" ] || die "找不到 node"
[ -f "$BENCH/progs/Hello.java" ] || die "bench/progs 不完整"

# 任务里提到「跑 exe 先 cp 到新目录」：带外监视器会锁执行过的 exe。
# 运行时我们从 bparv 复制一份到 bench/tmp/bin，主仓里的原件不动。
mkdir -p "$BENCH/tmp/bin" "$BENCH/raw" "$BENCH/logs" "$BENCH/out"
for b in rustjava run_jit run_jdk; do
  [ -f "$BENCH/tmp/bin/$b.exe" ] || cp -f "/d/vmless/bparv/$b.exe" "$BENCH/tmp/bin/" 2>/dev/null || true
done
# probe 用 tmp/bin 的副本更稳；但主仓 bparv 是「已交付二进制」，报告里按它算版本。
VML_BIN=$BPARV

# 版本指纹
{ "$JAVA" -version 2>&1 | head -2; } | sed 's/^/  /'
echo "  vmless: $("$VML_BIN/run_jdk.exe" 2>&1 | head -1 | cut -c1-40)"
"$VML_BIN/run_jit.exe" 'D:/vmless/bench/out/Empty/Empty.class' >/dev/null 2>&1 || true

# ---------------------------------------------------------------- 1 probe
say "1/6 测量核心 probe.exe"
if [ ! -f "$BENCH/tools/probe.exe" ] || [ "$REBUILD" = 1 ]; then
  [ -x "$CSC" ] || die "找不到 csc.exe，无法编译 probe"
  "$CSC" /nologo /optimize+ /target:exe /out:"D:\\vmless\\bench\\tools\\probe.exe" \
     "D:\\vmless\\bench\\tools\\probe.cs" >/dev/null || die "probe.cs 编译失败"
  echo "  已编译 probe.exe"
else
  echo "  复用已有 probe.exe"
fi

# ---------------------------------------------------------------- 2 基准程序
say "2/6 编译基准程序（javac 25）"
if [ "$REBUILD" = 1 ] || [ ! -d "$BENCH/out/Hello" ]; then
  rm -rf "$BENCH/out"; mkdir -p "$BENCH/out"
  for p in Empty Hello Big1000 Alloc8 Gc P2j P1L1 P1L2 P1L4 P1L8 ClassLoad2; do
    mkdir -p "$BENCH/out/$p"
    "$JAVAC" -d "$BENCH_W/out/$p" "$BENCH_W/progs/$p.java" || die "javac 失败: $p"
  done
  mkdir -p "$BENCH/out/ObjSize"
  for n in 1000000 2000000 4000000 8000000; do
    "$JAVAC" -d "$BENCH_W/out/ObjSize" "$BENCH_W/progs/ObjSize$n.java" || die "javac 失败: ObjSize$n"
  done
  mkdir -p "$BENCH/out/ObjSizeE"
  for n in 1000000 4000000 8000000; do
    "$JAVAC" -d "$BENCH_W/out/ObjSizeE" "$BENCH_W/progs/ObjSizeE$n.java" || die "javac 失败: ObjSizeE$n"
  done
  echo "  已编译 11 个程序 + 3 个对象大小变体"
else
  echo "  复用已有 out/（--rebuild 可强制重编）"
fi

# ---------------------------------------------------------------- 3 清场
say "3/6 清理上一轮原始数据"
if [ -n "$ONLY" ]; then
  # --only 时不清 raw/logs：否则重跑一节会把其它节的数据连带删掉（踩过）
  rm -f "$BENCH"/raw/manifest-*.json 2>/dev/null
  echo "  --only 模式：保留 raw/ 与 logs/，只覆盖本次跑的节"
else
  rm -f "$BENCH"/raw/*.jsonl "$BENCH"/raw/manifest-*.json "$BENCH"/logs/*.log 2>/dev/null
  echo "  raw/ 与 logs/ 已清空"
fi
# 机器负载会直接污染墙钟 —— 记录一份
LOAD=$(powershell.exe -NoProfile -Command \
  "(Get-CimInstance Win32_Processor | Measure-Object -Property LoadPercentage -Average).Average" 2>/dev/null | tr -d '\r\n ')
echo "  当前 CPU 负载: ${LOAD:-未知}%（跑基准期间机器上还有别的任务，见 README 的噪声说明）"

# ---------------------------------------------------------------- 4 跑批次
say "4/6 按维度跑（交错、可中断）"
BATCHES=$("$NODE" "$BENCH_W/tools/mkmanifest.mjs") || die "mkmanifest 失败"
echo "$BATCHES" | while IFS=$'\t' read -r reps manifest out envargs; do
  [ -z "${reps:-}" ] && continue
  section=$(basename "$manifest" | sed 's/^manifest-//; s/__.*//')
  if [ -n "$ONLY" ]; then
    case ",$ONLY," in *",$section,"*) ;; *) continue ;; esac
  fi
  [ "$QUICK" = 1 ] && reps=2
  printf '  %-42s reps=%-3s ' "$(basename "$manifest" .json)" "$reps"
  # shellcheck disable=SC2086
  if "$BENCH/tools/probe.exe" --manifest "$manifest" --out "$out" \
       --reps "$reps" --logdir "$BENCH_W/logs" --timeout 600000 $envargs 2>"$BENCH/logs/_probe.err"; then
    echo "ok"
  else
    echo "失败（见 logs/_probe.err）"; tail -3 "$BENCH/logs/_probe.err" | sed 's/^/      /'
  fi
done

# ---------------------------------------------------------------- 5 汇总
say "5/6 汇总"
if [ "$QUICK" = 1 ]; then
  echo "  --quick 模式不出正式报告（reps 太少）"
else
  "$NODE" "$BENCH_W/tools/summarize.mjs" || die "summarize 失败"
fi

# ---------------------------------------------------------------- 6 收尾
say "6/6 产物"
ls -1 "$BENCH/REPORT.md" "$BENCH/result.json" 2>/dev/null | sed 's/^/  /'
echo "  原始数据: $(ls "$BENCH"/raw/*.jsonl 2>/dev/null | wc -l) 个 jsonl，$(ls "$BENCH"/logs/*.log 2>/dev/null | wc -l) 个日志"
