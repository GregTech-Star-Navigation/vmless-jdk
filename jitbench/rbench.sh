#!/usr/bin/env bash
# 用法: bash rbench.sh <exe目录> [程序名...]
# 每个程序在自己的 iso/<name>/ 目录里跑（避免 load_class_path 把邻居类也编译进来）
# 取最后一行里出现的「毫秒数」作为程序自报耗时（P1L4/P2j 打印两行：sum, ms）
set -u
SRCDIR="$1"; shift
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
PROGS="${*:-P1L4 P2 Perf2 FibMain Q3 Q2 P1o}"
for prog in $PROGS; do
  for r in 1 2 3; do
    d="$BASE/tmp.$prog.$$.$r"; win="$WBASE/tmp.$prog.$$.$r"
    mkdir -p "$d"
    cp "$BASE/$SRCDIR/run_jit.exe" "$d/" || { echo "cp fail"; exit 1; }
    out=$(cd "$d" && MSYS_NO_PATHCONV=1 VMLESS_JDK=D:/vmless/patch/img timeout 120 ./run_jit.exe "$win/../iso/$prog/$prog.class" 2>&1)
    self=$(echo "$out" | grep -oE "(ms=|^)[0-9]+$" | grep -oE "[0-9]+" | tail -1)
    tot=$(echo "$out" | grep -o "执行耗时: [0-9.]*" | grep -o "[0-9.]*" | head -1)
    if [ -z "$tot" ] && [ -z "$self" ]; then echo "$prog ERR: $(echo "$out"|tail -2|tr '\n' ' ')"; break; fi
    printf "%s: self=%sms jit_total=%sms\n" "$prog" "${self:--}" "${tot:--}"
    rm -rf "$d"
  done
done
