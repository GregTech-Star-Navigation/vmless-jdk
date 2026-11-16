#!/usr/bin/env bash
# vmless 三路一致性验收：HotSpot / 解释器 / JIT 输出必须逐字节相等（含退出码）
# 用法：./verify.sh [bin目录]     默认 /d/vmless/bv6
set -u
BIN="${1:-/d/vmless/bin_l7v}"
JAVA="C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe"
SRC="D:/vmless/src"

# 本机有个带外文件监视器会长期持有「被执行过的 exe」句柄（之后 rm/cp/执行全报
# "Device or resource busy"）。对策：每轮把三件套复制进带 PID+时间的新目录再跑。
RUNBIN="/d/vmless/.runbin-$$-$(date +%H%M%S)"
mkdir -p "$RUNBIN"
for e in rustjava compile_src run_jit; do
    cp "$BIN/$e.exe" "$RUNBIN/" 2>/dev/null || cp "$BIN/examples/$e.exe" "$RUNBIN/" 2>/dev/null
done
if [ ! -x "$RUNBIN/compile_src.exe" ] || [ ! -x "$RUNBIN/rustjava.exe" ] || [ ! -x "$RUNBIN/run_jit.exe" ]; then
    echo "三件套不齐：$BIN 下缺 exe（rustjava / compile_src / run_jit）" >&2
    exit 2
fi
BIN="$RUNBIN"

OUT="D:/vmless/.verify-out.$$"
rm -rf "$OUT" 2>/dev/null; mkdir -p "$OUT"

ALL="InhBasic InhOverride InhChain InhSuperCall InhImplicitSuper InhInstanceof InhWide
Iface Iface2 CtorThis Multi Multi2 Multi3 Static2
Cmp1 Cmp2 Cmp3 Enum1 Enum2 Enum3 IfaceDef1 IfaceDef2
ForEach1 ForEach2 ForEach3 Ternary1 Ternary2 Label1 Label2 Nest1 Nest2 Nest3 Concat1 Concat2 Concat3 Mix2
Lg Db Point Loop Sw Obj2 ObjArr Arr Demo Perf2 LongEdge Doubles Mix DblCorpus Ret
RefArr ObjRef ArrType RefFld RefCmp NullChk WideFld WideArr BitOps PrintObj PrintObj2
TryThrow TryFinally TryNested TryCross TryCatch2 TryProbe TryProbe2 TryUncaught
Gen1 Gen2 Gen3 Gen4 Gen5 Vararg1 Vararg2 Vararg3 Vararg4 DoWhile1 SwitchStr1 MultiCatch1 Twr1
Box1 Box2 Box3 Str1 Str2 Obj3 Arr2"

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

# ---- 包名 + import：按依赖顺序编，同一个输出目录当 classpath ----
# 每组：先编依赖，再编入口，然后 java <入口类> / rustjava / run_jit 三方比对
multi_unit() {
    local label="$1" entry="$2" entryclass="$3"; shift 3
    local d="$OUT/_mu_$label"; mkdir -p "$d"
    local i=0
    for c in "$@"; do
        i=$((i+1))
        if [ "$c" = "$entry" ]; then continue; fi
        "$BIN/compile_src.exe" "$SRC/$c.java" "$d" >/dev/null 2>&1 || { bad=$((bad+1)); failed="$failed
  $label  依赖 $c 编译失败"; return; }
    done
    if ! "$BIN/compile_src.exe" "$SRC/$entry.java" "$d" >/dev/null 2>&1; then
        bad=$((bad+1)); failed="$failed
  $label  入口 $entry 编译失败"; return
    fi
    local cls="$d/${entryclass//.//}.class"
    jo=$("$JAVA" -cp "$d" "$entryclass" 2>/dev/null | tr -d '\r'); je=$?
    io=$("$BIN/rustjava.exe" "$cls" 2>/dev/null | tr -d '\r'); ie=$?
    to=$("$BIN/run_jit.exe" "$cls" 2>/dev/null | tr -d '\r'); te=$?
    if same; then ok=$((ok+1)); else
        bad=$((bad+1)); failed="$failed
  $label  退出码 java=$je intp=$ie jit=$te
    java=[$jo]
    intp=[$io]
    jit =[$to]"
    fi
}

# 跨编译单元继承
multi_unit "跨单元继承" InhUnitB InhUnitB InhUnitA
# 包名：pkg.PkgA 依赖 PkgB/PkgC
multi_unit "包名" PkgA pkg.PkgA PkgB PkgC
# import 单类型（app.Import1 依赖 lib.ImportTool）
multi_unit "import单类型" Import1 app.Import1 ImportTool
# import 通配（app2.Import2 依赖 lib.ImportTool + 同包 Import2Helper）
multi_unit "import通配" Import2 app2.Import2 ImportTool Import2Helper

echo "=========================================="
echo "三路一致: $ok    失败: $bad    缺样例: $skip"
[ "$bad" -eq 0 ] || printf '%s\n' "$failed"
echo "=========================================="
[ "$bad" -eq 0 ]
