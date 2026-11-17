#!/usr/bin/env bash
# 用法: bash rb2.sh <exe目录名> <程序名...>
set -u
SRCDIR="$1"; shift
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
for prog in "$@"; do
  for r in 1 2; do
    d="$BASE/run.$prog.$$.$r"; win="$WBASE/run.$prog.$$.$r"
    mkdir -p "$d"; cp "$BASE/$SRCDIR/run_jit.exe" "$d/" || { echo "cp fail"; exit 1; }
    out=$(cd "$d" && VMLESS_JDK=D:/vmless/patch/img timeout 120 ./run_jit.exe "$win/../$prog.class" 2>&1)
    echo "$prog r$r: $(echo "$out" | grep -E '执行耗时|sum=|Error|错误|失败' | tr '\n' ' ')"
    rm -rf "$d"
  done
done
