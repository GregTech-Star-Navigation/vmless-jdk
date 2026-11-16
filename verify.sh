#!/usr/bin/env bash
# vmless 三路一致性验收：HotSpot / 解释器 / JIT 输出必须逐字节相等（含退出码）
# 用法：./verify.sh [bin目录]     默认 /d/vmless/bv6
set -u
BIN="${1:-/d/vmless/bv6}"
JAVA="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe"
SRC="D:/vmless/src"
OUT="D:/vmless/.verify-out.112723"
rm -rf "$OUT" 2>/dev/null; mkdir -p "$OUT"

ALL="InhBasic InhOverride InhChain InhSuperCall InhImplicitSuper InhInstanceof InhWide
Iface Iface2 CtorThis Multi Multi2 Multi3 Static2
Lg Db Point Loop Sw Obj2 ObjArr Arr Demo Perf2 LongEdge Doubles Mix DblCorpus Ret
RefArr ObjRef ArrType RefFld RefCmp NullChk WideFld WideArr BitOps PrintObj PrintObj2
TryThrow TryFinally TryNested TryCross TryCatch2 TryProbe TryProbe2 TryUncaught"

# 跑一遍三方，回填全局 jo/io/to/je/ie/te
run_three() {
    local c="$1" d="$2" cls="$3"
    jo=$("$JAVA" -cp "$d" "$c" 2>/dev/null | tr -d '\r'); je=$?
    io=$("$BIN/rustjava.exe" "$cls" 2>/dev/null | tr -d '\r'); ie=$?
    to=$("$BIN/run_jit.exe" "$cls" 2>/dev/null | tr -d '\r'); te=$?
}
same() {
    [ "$jo" = "$io" ] && [ "$jo" = "$to" ] && [ "$je" = "$ie" ] && [ "$je" = "$te" ]
}

ok=0; bad=0; skip=0; failed=""
for c in $ALL; do
    [ -f "$SRC/$c.java" ] || { skip=$((skip+1)); continue; }
    d="$OUT/$c"; mkdir -p "$d"
    if ! "$BIN/compile_src.exe" "$SRC/$c.java" "$d" >/dev/null 2>&1; then
        bad=$((bad+1)); failed="$failed
  $c  编译失败"; continue
    fi
    cls="$d/$c.class"
    run_three "$c" "$d" "$cls"
    if same; then ok=$((ok+1)); continue; fi
    # 文件监视器抢句柄会造成偶发空输出，重试一次
    sleep 0.3
    run_three "$c" "$d" "$cls"
    if same; then ok=$((ok+1)); continue; fi
    bad=$((bad+1))
    failed="$failed
  $c  退出码 java=$je intp=$ie jit=$te
    java=[$(printf '%s' "$jo" | head -3 | tr '\n' '/')]
    intp=[$(printf '%s' "$io" | head -3 | tr '\n' '/')]
    jit =[$(printf '%s' "$to" | head -3 | tr '\n' '/')]"
done

# ---- 跨编译单元：父类在另一个 .java 里，先编 base 再编子类（同一个输出目录当 classpath）----
if [ -f "$SRC/InhUnitA.java" ] && [ -f "$SRC/InhUnitB.java" ]; then
    cu="$OUT/_crossunit"; mkdir -p "$cu"
    if "$BIN/compile_src.exe" "$SRC/InhUnitA.java" "$cu" >/dev/null 2>&1 \
       && "$BIN/compile_src.exe" "$SRC/InhUnitB.java" "$cu" >/dev/null 2>&1; then
        jo=$("$JAVA" -cp "$cu" InhUnitB 2>/dev/null | tr -d '\r'); je=$?
        io=$("$BIN/rustjava.exe" "$cu/InhUnitB.class" 2>/dev/null | tr -d '\r'); ie=$?
        to=$("$BIN/run_jit.exe" "$cu/InhUnitB.class" 2>/dev/null | tr -d '\r'); te=$?
        if same; then ok=$((ok+1)); else
            bad=$((bad+1)); failed="$failed
  InhUnit(跨单元)  退出码 java=$je intp=$ie jit=$te
    java=[$jo]
    intp=[$io]
    jit =[$to]"
        fi
    else
        bad=$((bad+1)); failed="$failed
  InhUnit(跨单元)  编译失败"
    fi
fi

# ---- 包名：pkg.PkgA/B/C 同包互引 + 全限定名 + 跨单元继承（必须按依赖顺序编）----
if [ -f "$SRC/PkgA.java" ] && [ -f "$SRC/PkgB.java" ] && [ -f "$SRC/PkgC.java" ]; then
    pd="$OUT/_pkg"; mkdir -p "$pd"
    pkgok=1
    for c in PkgB PkgC PkgA; do
        "$BIN/compile_src.exe" "$SRC/$c.java" "$pd" >/dev/null 2>&1 || pkgok=0
    done
    if [ "$pkgok" = 1 ]; then
        jo=$("$JAVA" -cp "$pd" pkg.PkgA 2>/dev/null | tr -d '\r'); je=$?
        io=$("$BIN/rustjava.exe" "$pd/pkg/PkgA.class" 2>/dev/null | tr -d '\r'); ie=$?
        to=$("$BIN/run_jit.exe" "$pd/pkg/PkgA.class" 2>/dev/null | tr -d '\r'); te=$?
        if same; then ok=$((ok+1)); else
            bad=$((bad+1)); failed="$failed
  Pkg(包名)  退出码 java=$je intp=$ie jit=$te
    java=[$jo]
    intp=[$io]
    jit =[$to]"
        fi
    else
        bad=$((bad+1)); failed="$failed
  Pkg(包名)  编译失败"
    fi
fi

echo "=========================================="
echo "三路一致: $ok    失败: $bad    缺样例: $skip"
[ "$bad" -eq 0 ] || printf '%s\n' "$failed"
echo "=========================================="
[ "$bad" -eq 0 ]
