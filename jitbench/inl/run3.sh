#!/usr/bin/env bash
# 内联样例的三路一致性：HotSpot / 解释器 / JIT 输出与退出码必须完全一致。
# 用法: bash run3.sh <bin目录>
set -u
BIN="$1"
BASE=/d/vmless/jitbench
WBASE=D:/vmless/jitbench
JAVA="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe"
JAVAC="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/javac.exe"
OUT="$BASE/inl/out"
rm -rf "$OUT"; mkdir -p "$OUT"
ok=0; bad=0
for f in "$BASE"/inl/*.java; do
  c=$(basename "$f" .java)
  d="$OUT/$c"; mkdir -p "$d"
  MSYS_NO_PATHCONV=1 "$JAVAC" -d "$WBASE/inl/out/$c" "$WBASE/inl/$c.java" >/dev/null 2>&1 || { echo "$c 编译失败"; bad=$((bad+1)); continue; }
  r=$(mktemp -d); mkdir -p "$r"
  cp "$BASE/$BIN/run_jit.exe" "$r/" || { echo "$c 复制 exe 失败"; bad=$((bad+1)); continue; }
  sleep 0.2
  cp /d/vmless/jitbench/binall/rustjava.exe "$r/" 2>/dev/null
  cls="$WBASE/inl/out/$c/$c.class"
  jo=$(MSYS_NO_PATHCONV=1 timeout 30 "$JAVA" -cp "$WBASE/inl/out/$c" "$c" 2>/dev/null | tr -d '\r'); je=$?
  io=$(cd "$r" && VMLESS_JDK=D:/vmless/patch/img timeout 30 ./rustjava.exe "$cls" 2>/dev/null | tr -d '\r'); ie=$?
  to=$(cd "$r" && MSYS_NO_PATHCONV=1 VMLESS_JDK=D:/vmless/patch/img $NINL timeout 30 ./run_jit.exe "$cls" 2>"$r/err.txt" | tr -d '\r'); te=$?
  inl=$(grep -o "内联 [0-9]*" "$r/err.txt" | head -1)
  if [ -n "$jo" ] && [ "$jo" = "$io" ] && [ "$jo" = "$to" ] && [ "$je" = "$ie" ] && [ "$je" = "$te" ]; then
    echo "$c  OK   ${inl:-内联 0}  行数=$(printf '%s' "$jo" | wc -l)"; ok=$((ok+1))
  else
    echo "$c  FAIL ${inl:-内联 0}"
    echo "  java[$je]=[$(printf '%s' "$jo" | head -6 | tr '\n' '/')]"
    echo "  intp[$ie]=[$(printf '%s' "$io" | head -6 | tr '\n' '/')]"
    echo "  jit [$te]=[$(printf '%s' "$to" | head -6 | tr '\n' '/')]"
    head -3 "$r/err.txt" | sed 's/^/    err: /'
    bad=$((bad+1))
  fi
done
echo "== 内联样例：一致 $ok / 失败 $bad =="
[ "$bad" -eq 0 ]
