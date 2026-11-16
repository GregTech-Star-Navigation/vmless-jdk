#!/usr/bin/env bash
# 样例一致性验收（CI 版）：同一批 .java → 解释器 / JIT 输出必须逐字节一致（含退出码）；
# 环境里有 JDK 25 时再补一路 HotSpot 做三方对照（找不到就退化成两方，脚本会明说）。
#
# 与仓库根 verify.sh 的分工：verify.sh 绑死本机路径与 106 个样例，供日常全量验收；
# 这个脚本路径全靠参数/环境变量，样例集随仓库走（rustjavac/samples），CI 与本地都能跑。
#
# 用法：
#   scripts/verify-samples.sh                              # 20 个代表样例
#   scripts/verify-samples.sh --all                        # samples/ 下全部样例
#   scripts/verify-samples.sh --samples /d/vmless/src --all # 本机完整语料
#   scripts/verify-samples.sh --bin <目录> --java <java.exe>
#
# 退出码：0 全过；1 有失败；2 用法/环境错误。
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/rustjavac/target/release"
SAMPLES="$ROOT/samples"
OUT="$ROOT/rustjavac/target/verify-out"
JAVA="${JAVA:-}"
ALL=0

while [ $# -gt 0 ]; do
    case "$1" in
        --all) ALL=1 ;;
        --bin) BIN="$2"; shift ;;
        --samples) SAMPLES="$2"; shift ;;
        --java) JAVA="$2"; shift ;;
        --out) OUT="$2"; shift ;;
        -h|--help) sed -n '1,16p' "$0"; exit 0 ;;
        *) echo "未知参数: $1" >&2; exit 2 ;;
    esac
    shift
done

# JDK：--java > $JAVA > JAVA_HOME > PATH。找不到只跑两方对照（不算失败）。
if [ -z "$JAVA" ]; then
    if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java.exe" ]; then
        JAVA="$JAVA_HOME/bin/java.exe"
    elif [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java" ]; then
        JAVA="$JAVA_HOME/bin/java"
    elif command -v java >/dev/null 2>&1; then
        JAVA=$(command -v java)
    fi
fi

# 三个可执行文件：rustjava 在 bin，compile_src / run_jit 是 examples —— 两处都认
resolve_exe() {
    for c in "$BIN/$1.exe" "$BIN/examples/$1.exe"; do
        [ -x "$c" ] && { printf '%s' "$c"; return 0; }
    done
    return 1
}
# 传给 Windows 程序（含 java.exe）的路径必须是盘符形式：Git Bash 的 /d/... 会被当成 D:\d\...
winpath() {
    if command -v cygpath >/dev/null 2>&1; then cygpath -m "$1"; else printf '%s' "$1"; fi
}
RUSTJAVA=$(resolve_exe rustjava) || { echo "缺少 rustjava.exe（找过 $BIN 与 $BIN/examples）" >&2; exit 2; }
COMPILE=$(resolve_exe compile_src) || { echo "缺少 compile_src.exe（cargo build --release --example compile_src）" >&2; exit 2; }
RUNJIT=$(resolve_exe run_jit) || { echo "缺少 run_jit.exe（cargo build --release --example run_jit）" >&2; exit 2; }
[ -d "$SAMPLES" ] || { echo "样例目录不存在: $SAMPLES" >&2; exit 2; }

rm -rf "$OUT" 2>/dev/null
mkdir -p "$OUT"

# 默认 20 个代表样例，每个覆盖一条独立能力线：
# 算术/循环/递归/比较/switch(String)/do-while/for-each/标签跳转/try-finally/
# 多重 catch/try-with-resources/继承虚分派/接口 default/泛型擦除/变参/
# 字符串拼接/嵌套类型/多维数组/枚举/静态字段与 <clinit>
SUBSET="Calc Loop FibMain Cmp1 SwitchStr1 DoWhile1 ForEach1 Label1
TryFinally MultiCatch1 Twr1 InhOverride IfaceDef1 Gen1 Vararg1
Concat1 Nest1 Multi Enum1 Static2"

# 多编译单元组（入口:入口内部名:依赖，按编译顺序）。样例目录里凑不齐就跳过。
MULTI_UNITS="PkgA:pkg.PkgA:PkgB PkgC
Import1:app.Import1:ImportTool
Import2:app2.Import2:ImportTool Import2Helper
InhUnitB:InhUnitB:InhUnitA"

# --all：样例目录下全部 .java（多单元组的辅助类由组负责跑，不单独跑）
if [ "$ALL" = "1" ]; then
    LIST=""
    for f in "$SAMPLES"/*.java; do
        LIST="$LIST $(basename "$f" .java)"
    done
    LIST=$(printf '%s' "$LIST" | tr ' ' '\n' | grep -vxE 'PkgB|PkgC|ImportTool|Import2Helper|InhUnitA' | tr '\n' ' ')
else
    LIST="$SUBSET"
fi

ok=0; bad=0; skip=0; failed=""

# 解释器 / JIT / （可选）HotSpot 三路逐字节比对。
# 退出码必须单独取：管道里最后一个命令是 tr，`$(... | tr ...); $?` 拿到的是 tr 的状态。
check() {
    local label="$1" cls="$2" cp="$3"
    local ie te je=0 out_i out_t jo="" t
    t="$OUT/.o.$$"
    "$RUSTJAVA" "$(winpath "$cls")" >"$t" 2>/dev/null; ie=$?
    out_i=$(tr -d '\r' <"$t")
    "$RUNJIT" "$(winpath "$cls")" >"$t" 2>/dev/null; te=$?
    out_t=$(tr -d '\r' <"$t")
    if [ -n "$JAVA" ]; then
        "$JAVA" -cp "$(winpath "$cp")" "$label" >"$t" 2>/dev/null; je=$?
        jo=$(tr -d '\r' <"$t")
        rm -f "$t"
        if [ "$jo" = "$out_i" ] && [ "$jo" = "$out_t" ] && [ "$je" = "$ie" ] && [ "$je" = "$te" ]; then
            return 0
        fi
    else
        rm -f "$t"
        if [ "$out_i" = "$out_t" ] && [ "$ie" = "$te" ]; then
            return 0
        fi
    fi
    failed="$failed
  $label  退出码 java=$je 解释器=$ie JIT=$te
    java =[$(printf '%s' "$jo" | head -3 | tr '\n' '/')]
    intp =[$(printf '%s' "$out_i" | head -3 | tr '\n' '/')]
    jit  =[$(printf '%s' "$out_t" | head -3 | tr '\n' '/')]"
    return 1
}

compile_into() { # 源文件 输出目录
    "$COMPILE" "$(winpath "$1")" "$(winpath "$2")" >/dev/null 2>&1
}

echo "bin   = $BIN"
echo "samples = $SAMPLES"
if [ -n "$JAVA" ]; then echo "jdk   = $JAVA"; else echo "jdk   = （未找到，只做解释器/JIT 两方对照）"; fi

for s in $LIST; do
    src="$SAMPLES/$s.java"
    [ -f "$src" ] || { skip=$((skip + 1)); echo "  跳过 $s（无此样例）"; continue; }
    d="$OUT/$s"; mkdir -p "$d"
    if ! compile_into "$src" "$d"; then
        bad=$((bad + 1)); failed="$failed
  $s  编译失败"
        continue
    fi
    if check "$s" "$d/$s.class" "$d"; then ok=$((ok + 1)); else bad=$((bad + 1)); fi
done

# 多编译单元：依赖先编进同一目录，该目录同时当 classpath
# 用 here-string 而不是管道 —— 管道的 while 在子 shell 里跑，计数会丢
while IFS=: read -r entry inner deps; do
    [ -n "${entry:-}" ] || continue
    ready=1; missing=""
    for need in $entry $deps; do
        [ -f "$SAMPLES/$need.java" ] || { missing="$missing $need.java"; ready=0; }
    done
    [ "$ready" = "1" ] || { echo "  跳过组 $entry（样例目录缺:$missing）"; continue; }
    d="$OUT/_g_$entry"; mkdir -p "$d"
    broke=0
    for dep in $deps; do
        compile_into "$SAMPLES/$dep.java" "$d" || { bad=$((bad + 1)); failed="$failed
  组 $entry  依赖 $dep 编译失败"; broke=1; break; }
    done
    [ "$broke" = "1" ] && continue
    if ! compile_into "$SAMPLES/$entry.java" "$d"; then
        bad=$((bad + 1)); failed="$failed
  组 $entry  入口编译失败"
        continue
    fi
    if check "$inner" "$d/${inner//.//}.class" "$d"; then ok=$((ok + 1)); else bad=$((bad + 1)); fi
done <<< "$MULTI_UNITS"
rm -f "$OUT/.o.$$"

echo "=========================================="
if [ -n "$JAVA" ]; then
    echo "三方一致: $ok    失败: $bad    缺样例: $skip"
else
    echo "解释器/JIT 两方一致: $ok    失败: $bad    缺样例: $skip"
fi
[ "$bad" -eq 0 ] || printf '%s\n' "$failed"
echo "=========================================="
[ "$bad" -eq 0 ]
