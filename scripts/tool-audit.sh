#!/usr/bin/env bash
# 工具对抗性体检：15 个工具 × 8 类坏输入，逐条记录 退出码 / 是否 panic / stderr 首行。
#
#   bash scripts/tool-audit.sh                     # 用 PATH 里的工具（或 btools）
#   BIN=/d/vmless/btools bash scripts/tool-audit.sh
#   bash scripts/tool-audit.sh > audit.tsv         # 落成 TSV 存档
#
# 判定标准（人工看这三列就够）：
#   exit   : 0 = 坏输入被当成功（有问题） 1 = 运行错 2 = 用法错 124 = 挂住
#   panic  : 1 = 出现 `panicked at`（硬红线，必须修）
#   stderr : 空的 stderr = 用户看不到原因
#
# 这台机器上跑一遍约 2~4 分钟（大部分时间花在故意挂住的常驻服务上，每个 15 秒超时）。
set -u

BIN=${BIN:-/d/vmless/btools}
export PATH="$BIN:$PATH"
CACHE=${CACHE:-/d/vmless/bench/tmp/fixtures}   # 大文件 fixture 复用，别每次重建
mkdir -p "$CACHE"
F="D:/vmless/bench/tmp/fixtures"              # 给 Windows 程序看的路径形式
FL=$CACHE                                      # 给 bash 看的路径形式

# ---- fixtures ---------------------------------------------------------------
: > "$FL/empty.bin"
head -c 40  /d/vmless/bench/out/Hello/Hello.class > "$FL/trunc.class" 2>/dev/null || \
  printf '\xCA\xFE\xBA\xBEgarbagegarbagegarbagegarbage' > "$FL/trunc.class"
head -c 5000 '/d/vmless/graalvm-jdk-25.0.4+7.1/jmods/java.base.jmod' > "$FL/trunc.jmod" 2>/dev/null || \
  printf 'PK\x03\x04truncated' > "$FL/trunc.jmod"
head -c 2048 '/c/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules' > "$FL/trunc.modules" 2>/dev/null || \
  printf '\xDA\xDA\xFE\xCAtruncated' > "$FL/trunc.modules"
mkdir -p "$FL/rodir" "$FL/ademo"
cp /d/vmless/bench/out/Hello/Hello.class "$FL/ademo/" 2>/dev/null || true
LONGREL="$(printf 'a%.0s' {1..120})/$(printf 'b%.0s' {1..120})/f.class"
LONG="$F/$LONGREL"
NOPE="$F/no/such/file.bin"

chmod 555 "$FL/rodir" 2>/dev/null
cp /d/vmless/bench/out/Hello/Hello.class "$FL/noread.class" 2>/dev/null || true
icacls "D:\\vmless\\bench\\tmp\\fixtures\\noread.class" /deny "Everyone:(R)" >/dev/null 2>&1
icacls "D:\\vmless\\bench\\tmp\\fixtures\\rodir" /deny "Everyone:(W)" >/dev/null 2>&1

run() { # run <tool> <case> <args...>
  local tool=$1 case=$2; shift 2
  local err rc
  err=$(timeout 15 "$@" 2>&1 >/dev/null); rc=$?
  local panic=0
  case "$err" in *"panicked at"*) panic=1 ;; esac
  printf '%s\t%s\t%s\t%s\t%s\n' "$tool" "$case" "$rc" "$panic" \
    "$(printf '%s' "$err" | head -1 | tr '\t' ' ' | cut -c1-110)"
}

echo "tool	case	exit	panic	stderr"

# 每个工具的「吃一个路径」的调用形式：%P 换成 fixture 路径
run jimage  nonexistent   jimage list "$NOPE"
run jimage  empty-file    jimage list "$F/empty.bin"
run jimage  truncated     jimage list "$F/trunc.modules"
run jimage  extract-ro-dir jimage extract --dir "$F/rodir" "C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules"
run jimage  long-path     jimage list "$LONG"
run jimage  bad-option    jimage list --definitely-not-an-option "C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules"
run jimage  no-read-perm  jimage list "$F/noread.class"

run jmod    nonexistent   jmod list "$NOPE"
run jmod    empty-file    jmod list "$F/empty.bin"
run jmod    truncated     jmod list "$F/trunc.jmod"
run jmod    create-ro-dir jmod create --class-path "$F/ademo" --dir "$F/rodir" "$F/out.jmod"
run jmod    long-path     jmod list "$LONG"
run jmod    bad-option    jmod list --definitely-not-an-option "$F/trunc.jmod"
run jmod    no-read-perm  jmod list "$F/noread.class"

run serialver nonexistent  serialver -classpath "$NOPE" Hello
run serialver empty-file    serialver -classpath "$F/empty.bin" Hello
run serialver truncated     serialver -classpath "$F/trunc.class" Hello
run serialver long-path     serialver -classpath "$LONG" Hello
run serialver bad-option    serialver --definitely-not-an-option Hello
run serialver no-read-perm  serialver -classpath "$F/noread.class" Hello

run jnativescan nonexistent  jnativescan --class-path "$NOPE"
run jnativescan empty-file    jnativescan --class-path "$F/empty.bin"
run jnativescan truncated     jnativescan --class-path "$F/trunc.class"
run jnativescan long-path     jnativescan --class-path "$LONG"
run jnativescan bad-option    jnativescan --definitely-not-an-option

run jdeprscan nonexistent   jdeprscan "$NOPE"
run jdeprscan empty-file     jdeprscan "$F/empty.bin"
run jdeprscan truncated      jdeprscan "$F/trunc.class"
run jdeprscan long-path      jdeprscan "$LONG"
run jdeprscan bad-option     jdeprscan --definitely-not-an-option

run javaw   nonexistent    javaw "$NOPE"
run javaw   empty-file      javaw "$F/empty.bin"
run javaw   truncated       javaw "$F/trunc.class"
run javaw   long-path       javaw "$LONG"
run javaw   bad-option      javaw --definitely-not-an-option

# 端口占用：监听方必须绑**同一个地址**才能稳定复现（Windows 的 SO_REUSEADDR 语义）
node -e 'require("net").createServer().listen(18080,"127.0.0.1")' &
PORT_PID=$!
sleep 1
run jwebserver nonexistent  jwebserver -d "$NOPE" -p 18081
run jwebserver empty-file   jwebserver -d "$F/empty.bin" -p 18082
run jwebserver bad-option   jwebserver --definitely-not-an-option
run jwebserver port-in-use  timeout 8 jwebserver -d "$F/ademo" -p 18080
run jwebserver long-path    jwebserver -d "$LONG" -p 18083
run jwebserver no-read-perm jwebserver -d "$F/noread.class" -p 18084
kill $PORT_PID 2>/dev/null

run jlink   nonexistent    jlink --module-path "$NOPE" --add-modules java.base --output "$F/linkout1"
run jlink   empty-file     jlink --module-path "$F/empty.bin" --add-modules java.base --output "$F/linkout2"
run jlink   truncated      jlink --module-path "$F/trunc.modules" --add-modules java.base --output "$F/linkout3"
run jlink   output-ro-dir  jlink --module-path "C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/lib/modules" --add-modules java.base --output "$F/rodir/linkout"
run jlink   long-path      jlink --module-path "$LONG" --add-modules java.base --output "$F/linkout4"
run jlink   bad-option     jlink --definitely-not-an-option

run jshell  nonexistent    jshell "$NOPE"
run jshell  empty-file     jshell "$F/empty.bin"
run jshell  truncated      jshell "$F/trunc.class"
run jshell  long-path      jshell "$LONG"
run jshell  bad-option     jshell --definitely-not-an-option

run javadoc nonexistent    javadoc -d "$F/docout1" "$NOPE"
run javadoc empty-file     javadoc -d "$F/docout2" "$F/empty.bin"
run javadoc truncated      javadoc -d "$F/docout3" "$F/trunc.class"
run javadoc output-ro-dir  javadoc -d "$F/rodir/doc" "$F/ademo/Hello.class"
run javadoc long-path      javadoc -d "$F/docout4" "$LONG"
run javadoc bad-option     javadoc --definitely-not-an-option

run jpackage nonexistent   jpackage --input "$NOPE" --name demo --main-jar x.jar --type app-image
run jpackage empty-file    jpackage --input "$F/empty.bin" --name demo --main-jar x.jar --type app-image
run jpackage bad-option    jpackage --definitely-not-an-option
run jpackage long-path     jpackage --input "$LONG" --name demo --main-jar x.jar --type app-image

run jrunscript nonexistent jrunscript "$NOPE"
run jrunscript empty-file  jrunscript "$F/empty.bin"
run jrunscript truncated   jrunscript "$F/trunc.class"
run jrunscript long-path   jrunscript "$LONG"
run jrunscript bad-option  jrunscript --definitely-not-an-option

run rmiregistry bad-option rmiregistry --definitely-not-an-option
run rmiregistry bad-port   rmiregistry notanumber
# 端口占用（绑同一地址）
node -e 'require("net").createServer().listen(18085,"0.0.0.0")' &
P2=$!
sleep 1
run rmiregistry port-in-use timeout 8 rmiregistry 18085
kill $P2 2>/dev/null

run jps     nonexistent    jps -l "$NOPE"
run jps     bad-option     jps --definitely-not-an-option

run jdb     nonexistent    jdb -classpath "$NOPE" Hello
run jdb     empty-file     jdb -classpath "$F/empty.bin" Hello
run jdb     truncated      jdb -classpath "$F/trunc.class" Hello
run jdb     long-path      jdb -classpath "$LONG" Hello
run jdb     bad-option     jdb --definitely-not-an-option Hello

# 收尾：把 ACL 与只读位撤掉，免得下次跑不动
chmod 755 "$FL/rodir" 2>/dev/null
icacls "D:\\vmless\\bench\\tmp\\fixtures\\noread.class" /remove:d "Everyone" >/dev/null 2>&1
icacls "D:\\vmless\\bench\\tmp\\fixtures\\rodir" /remove:d "Everyone" >/dev/null 2>&1
