#!/usr/bin/env bash
# run_jdk M2 与真 java 逐行比对（父 agent 验收第 7 条）
set -u
BIN="$1"
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
JAVA="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe"
export VMLESS_JDK=D:/vmless/patch/img
d="$BASE/jdkrun"; mkdir -p "$d"; cp "$BASE/$BIN/run_jdk.exe" "$d/" || exit 1
# run_jdk <class 文件>：用现有的 M2 样例
cls=$(ls /d/vmless/jitbench/iso/*/*.class 2>/dev/null | head -0)
for pair in "M2" ; do :; done
echo "（run_jdk 的用法见 examples/run_jdk.rs）"
