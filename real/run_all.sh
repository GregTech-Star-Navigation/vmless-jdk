#!/usr/bin/env bash
# 真程序样张：编译 -> 真 java 跑 -> vmless 跑 -> 逐字节比对
# 用法: bash real/run_all.sh <三件套目录>
set -u
BIN=${1:?用法: run_all.sh <三件套目录>}
ROOT=D:/vmless/real
JAVA="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe"
export VMLESS_JDK=${VMLESS_JDK:-D:/vmless/patch/img}
pass=0; fail=0; failed=""
for src in "$ROOT"/src/*.java; do
  n=$(basename "$src" .java)
  out="$ROOT/out/$n"; rm -rf "$out"; mkdir -p "$out"
  cerr=$("$BIN/compile_src.exe" "$src" "$out" 2>&1)
  if [ $? -ne 0 ]; then
    fail=$((fail+1)); failed="$failed $n(compile)"
    msg=$(printf '%s' "$cerr" | tr '\n' ' ' | sed 's/.*编译失败: //' | cut -c1-60)
    printf "  %-4s 编译失败: %s\n" "$n" "$msg"
    continue
  fi
  j=$("$JAVA" -cp "$out" "$n" 2>&1 | tr -d '\r'; echo "rc=$?")
  v=$(cd /d/vmless && timeout 180 "$BIN/run_jdk.exe" "$out" "$n" 2>&1 | grep -avE '^java.base|^类表|^退出值|^缺 native' | tr -d '\r'; echo "rc=$?")
  if [ "$j" = "$v" ]; then
    pass=$((pass+1)); printf "  %-4s OK\n" "$n"
  else
    fail=$((fail+1)); failed="$failed $n(out)"
    printf "  %-4s 不一致\n    java  : %s\n    vmless: %s\n" "$n" "$(printf '%s' "$j" | head -3 | tr '\n' '|')" "$(printf '%s' "$v" | head -3 | tr '\n' '|')"
  fi
done
echo "真程序样张: 通过 $pass / 失败 $fail${failed:+  [$failed ]}"
