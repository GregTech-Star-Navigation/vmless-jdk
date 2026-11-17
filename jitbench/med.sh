#!/usr/bin/env bash
# 多次采样取中位（用 run_jit 自己的「执行耗时」，它打在 stderr）
# bash med.sh <bin> <prog> [次数]
set -u
BIN="$1"; PROG="$2"; N="${3:-7}"
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
for i in $(seq 1 $N); do
  d=$(mktemp -d); cp "$BASE/$BIN/run_jit.exe" "$d/" || exit 1
  tot=$(cd "$d" && MSYS_NO_PATHCONV=1 VMLESS_JDK=D:/vmless/patch/img timeout 120 ./run_jit.exe "$WBASE/iso/$PROG/$PROG.class" 2>&1 >/dev/null | grep -o "执行耗时: [0-9.]*" | grep -o "[0-9.]*")
  echo "${tot:-FAIL}"
  rm -rf "$d"
done | sort -n | awk '{a[NR]=$1} END{printf "n=%d min=%s median=%s max=%s\n", NR, a[1], a[int((NR+1)/2)], a[NR]}'
