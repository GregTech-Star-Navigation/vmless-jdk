#!/usr/bin/env bash
# 用法: bash rb.sh <exe目录名> [重复次数]
# 每轮把 run_jit.exe 复制到新目录再跑（带外监视器会锁住执行过的 exe）
set -u
SRCDIR="$1"
REPS="${2:-3}"
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
for prog in P1 P2 Perf2 FibMain; do
  for r in $(seq 1 "$REPS"); do
    d="$BASE/run.$prog.$$.$r"
    win="$WBASE/run.$prog.$$.$r"
    mkdir -p "$d"
    cp "$BASE/$SRCDIR/run_jit.exe" "$d/" || { echo "cp fail"; exit 1; }
    cp "$BASE/$prog.class" "$d/"
    out=$(cd "$d" && timeout 120 ./run_jit.exe "$win/$prog.class" 2>&1)
    ms=$(echo "$out" | grep -o "执行耗时: [0-9.]*" | grep -o "[0-9.]*" | head -1)
    interp=$(echo "$out" | grep -o "解释 [0-9]*" | head -1)
    if [ -z "$ms" ]; then ms="FAIL"; echo "$out" | head -5; fi
    printf "%s r%s: %s ms%s\n" "$prog" "$r" "$ms" "${interp:+ [$interp]}"
    rm -rf "$d"
  done
done
