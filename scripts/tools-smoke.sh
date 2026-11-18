#!/usr/bin/env bash
# JDK 工具集冒烟：18 项检查，全是对着**真跑起来的目标**（vmsim / 本地 KDC / 本地 keytab）做的，
# 不是 --help。每一项打印 PASS/FAIL + 一行实际输出，最后给汇总。
#
#   bash scripts/tools-smoke.sh                     # 用 /d/vmless/btools 里的 exe
#   BIN=/path/to/bin bash scripts/tools-smoke.sh    # 用别处的 exe（打包产物、crate 的 target）
#
# 为什么要有这个脚本：这 17 个工具在 2026-11-19 之前是「打印一句不提供然后 exit 1」的桩，
# 挡住了兄弟 crate 里的真实现。桩最容易的回归方式不是编译失败，而是**悄悄又被加回来**
# —— 所以判据不能是「编过了」，得是「对着活目标跑出真数据」。
#
# 依赖：只有 bash + 这套 exe。GUI 那几项需要一个可交互桌面（没有的话会 FAIL 并说明原因）。
set -u

BIN=${BIN:-/d/vmless/btools}
export PATH="$BIN:$PATH"
W=$(mktemp -d)
FAILED=0
PASSED=0
PIDS=()

cleanup() {
    for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
    for p in $(ps -W 2>/dev/null | grep -E "[v]msim|[j]statd" | awk '{print $1}'); do kill "$p" 2>/dev/null; done
    rm -rf "$W"
}
trap cleanup EXIT

ok()   { PASSED=$((PASSED+1)); printf 'PASS  %-22s %s\n' "$1" "$2"; }
bad()  { FAILED=$((FAILED+1)); printf 'FAIL  %-22s %s\n' "$1" "$2"; }
# first <行数>：取前 n 行里最后一条非空；用来从输出里挑一行给人看
head1() { grep -v '^[[:space:]]*$' | head -1; }

need() { [ -x "$BIN/$1.exe" ] || { bad "$1" "找不到 $BIN/$1.exe"; return 1; }; }

for t in jps jstack jmap jstat jcmd jinfo jstatd keytool jarsigner ktab kinit klist kdc \
         jfr jhsdb jconsole jabswitch jaccesswalker jaccessinspector vmsim; do
    [ -x "$BIN/$t.exe" ] || { echo "缺 $BIN/$t.exe —— 先构建/BIN= 指过去"; exit 2; }
done

# ---------------------------------------------------------------- 活目标：vmsim
( cd "$W" && "$BIN/vmsim.exe" --main com.example.GcBench --arg -Xmx64m --threads 4 \
    --objects 2000 --gc-interval 800 > vmsim.log 2>&1 & echo $! > vmsim.pid )
VM=$(cat "$W/vmsim.pid")
PIDS+=("$VM")
for _ in $(seq 1 40); do grep -q '^perfdata = ' "$W/vmsim.log" 2>/dev/null && break; sleep 0.25; done
PID=$(sed -n 's/^vmsim pid=\([0-9]*\).*/\1/p' "$W/vmsim.log")
[ -n "${PID:-}" ] || { echo "vmsim 没起来"; cat "$W/vmsim.log"; exit 2; }

# ---------------------------------------------------------------- 1-7 进程/诊断族
if out=$(timeout 20 "$BIN/jps.exe" -l 2>&1) && printf '%s' "$out" | grep -q "$PID com.example.GcBench"; then
    ok jps "$(printf '%s' "$out" | grep "$PID ")"; else bad jps "${out:-无输出}"; fi

if out=$(timeout 20 "$BIN/jstack.exe" "$PID" 2>&1) && printf '%s' "$out" | grep -q 'at com.example.GcBench.main(Main.java:'; then
    ok jstack "$(printf '%s' "$out" | grep 'at com.example' | head1)"; else bad jstack "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 20 "$BIN/jmap.exe" -histo "$PID" 2>&1) && printf '%s' "$out" | grep -q 'com.example.'; then
    ok jmap "$(printf '%s' "$out" | grep 'com.example.' | head1)"; else bad jmap "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 20 "$BIN/jstat.exe" -gc "$PID" 2>&1) && printf '%s' "$out" | grep -q 'YGC'; then
    ok jstat "YGC=$(printf '%s' "$out" | tail -1 | awk '{print $13}')"
else bad jstat "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 20 "$BIN/jcmd.exe" "$PID" VM.version 2>&1) && printf '%s' "$out" | grep -qi 'vmless'; then
    ok "jcmd VM.version" "$(printf '%s' "$out" | grep -i vmless | head1)"
else bad "jcmd VM.version" "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 20 "$BIN/jcmd.exe" "$PID" GC.heap_info 2>&1) && printf '%s' "$out" | grep -q 'heap'; then
    ok "jcmd GC.heap_info" "$(printf '%s' "$out" | grep -i heap | head1)"
else bad "jcmd GC.heap_info" "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 20 "$BIN/jinfo.exe" "$PID" 2>&1) && printf '%s' "$out" | grep -q 'java.vm.name'; then
    ok jinfo "$(printf '%s' "$out" | grep 'java.vm.name' | head1)"; else bad jinfo "$(printf '%s' "$out" | head1)"; fi

# ---------------------------------------------------------------- 8 jstatd（远程取数）
PORT=$((19000 + (RANDOM % 2000)))
( cd "$W" && "$BIN/jstatd.exe" -p "$PORT" > jstatd.log 2>&1 & echo $! > jstatd.pid )
PIDS+=("$(cat "$W/jstatd.pid")")
sleep 1.5
if out=$(timeout 20 "$BIN/jstat.exe" -gc "$PID@127.0.0.1:$PORT" 2>&1) && printf '%s' "$out" | grep -q 'YGC'; then
    ok jstatd "jstat pid@127.0.0.1:$PORT → YGC=$(printf '%s' "$out" | tail -1 | awk '{print $13}')"
else bad jstatd "$(printf '%s' "$out" | head1)"; fi

# ---------------------------------------------------------------- 9-11 密钥/签名
cd "$W" || exit 2
KS=smoke.p12
if out=$(timeout 90 "$BIN/keytool.exe" -genkeypair -alias smoke -keyalg RSA -keysize 2048 \
        -dname "CN=smoke.example, O=VMless" -validity 30 -keystore $KS -storepass changeit 2>&1) \
   && out2=$(timeout 60 "$BIN/keytool.exe" -list -keystore $KS -storepass changeit 2>&1) \
   && printf '%s' "$out2" | grep -q 'smoke, '; then
    ok keytool "$(printf '%s' "$out2" | grep '证书指纹' | head1)"
else bad keytool "$(printf '%s%s' "${out:-}" "${out2:-}" | head1)"; fi

# 签名目标：造一个最小 jar。优先用 jar/zip，两者都没有就用 PowerShell 的 Compress-Archive
# （zip 就是 jar 的容器格式，jarsigner 只认 ZIP 结构 + MANIFEST，没有也能签）。
if [ ! -f sign.jar ]; then
    mkdir -p cls && printf 'x' > cls/a.txt
    if   command -v jar >/dev/null 2>&1; then jar cf sign.jar -C cls .
    elif command -v zip >/dev/null 2>&1; then ( cd cls && zip -q ../sign.jar a.txt )
    elif command -v pwsh >/dev/null 2>&1; then
        pwsh -NoProfile -Command "Compress-Archive -Path '$W/cls/a.txt' -DestinationPath '$W/sign.zip' -Force; \
            Move-Item '$W/sign.zip' '$W/sign.jar' -Force" >/dev/null 2>&1
    fi
fi
if [ -f sign.jar ] && out=$(timeout 90 "$BIN/jarsigner.exe" -keystore $KS -storepass changeit sign.jar smoke 2>&1) \
   && out2=$(timeout 90 "$BIN/jarsigner.exe" -verify sign.jar 2>&1) \
   && printf '%s' "$out2" | grep -q '已验证'; then
    ok jarsigner "$(printf '%s' "$out2" | grep '已验证' | head1)"
else bad jarsigner "$(printf '%s%s' "${out:-}" "${out2:-}" | head1)"; fi

# ---------------------------------------------------------------- 12-14 Kerberos
if out=$(timeout 30 "$BIN/ktab.exe" -a smoke@SMOKE.LOCAL -k smoke.keytab -p smoke-pw 2>&1) \
   && out2=$(timeout 30 "$BIN/ktab.exe" -l -k smoke.keytab 2>&1) \
   && printf '%s' "$out2" | grep -q 'smoke@SMOKE.LOCAL'; then
    ok ktab "$(printf '%s' "$out2" | grep 'smoke@' | head1)"
else bad ktab "$(printf '%s%s' "${out:-}" "${out2:-}" | head1)"; fi

timeout 30 "$BIN/ktab.exe" -a krbtgt/SMOKE.LOCAL@SMOKE.LOCAL -k smoke.keytab -p krbtgt-pw >/dev/null 2>&1
KPORT=$((21000 + (RANDOM % 2000)))
( cd "$W" && "$BIN/kdc.exe" --realm SMOKE.LOCAL --port "$KPORT" --keytab smoke.keytab > kdc.log 2>&1 & echo $! > kdc.pid )
PIDS+=("$(cat "$W/kdc.pid")")
sleep 1.5
if out=$(timeout 30 "$BIN/kinit.exe" -k -t smoke.keytab smoke@SMOKE.LOCAL -S "127.0.0.1:$KPORT" -c "$W/cc" 2>&1) \
   && out2=$(timeout 30 "$BIN/klist.exe" -c "$W/cc" 2>&1) \
   && printf '%s' "$out2" | grep -q 'krbtgt/SMOKE.LOCAL@SMOKE.LOCAL'; then
    ok kinit "$(printf '%s' "$out" | grep '已拿到 TGT' | head1)"
    ok klist "$(printf '%s' "$out2" | grep 'krbtgt/' | head1)"
else
    bad kinit "$(printf '%s' "${out:-}" | head1)"; bad klist "$(printf '%s' "${out2:-}" | head1)"
fi

# ---------------------------------------------------------------- 15-16 JFR / 堆转储
if out=$(timeout 90 "$BIN/jfr.exe" record smoke.jfr --pid "$PID" --events 200 2>&1) && [ -s smoke.jfr ]; then
    if out2=$(timeout 60 "$BIN/jfr.exe" summary smoke.jfr 2>&1) \
       && out3=$(timeout 60 "$BIN/jfr.exe" print smoke.jfr 2>&1) \
       && printf '%s' "$out3" | grep -q 'jdk.ExecutionSample'; then
        ok jfr "$(printf '%s' "$out2" | grep 'Version:' | head1) / $(printf '%s' "$out3" | grep -c 'jdk\.') 类事件"
    else bad jfr "$(printf '%s' "${out2:-}" | head1)"; fi
else bad jfr "$(printf '%s' "${out:-}" | head1)"; fi

if out=$(timeout 40 "$BIN/jhsdb.exe" jstack --pid "$PID" 2>&1) && printf '%s' "$out" | grep -q 'at com.example'; then
    ok jhsdb "$(printf '%s' "$out" | grep 'at com.example' | head1)"
else bad jhsdb "$(printf '%s' "$out" | head1)"; fi

# ---------------------------------------------------------------- 17-20 无障碍 / GUI
if out=$(timeout 20 "$BIN/jabswitch.exe" -q 2>&1) && printf '%s' "$out" | grep -q 'Java Access Bridge'; then
    ok jabswitch "$(printf '%s' "$out" | head1)"; else bad jabswitch "$(printf '%s' "$out" | head1)"; fi

if out=$(timeout 40 "$BIN/jaccesswalker.exe" 2>&1) && printf '%s' "$out" | grep -q '个窗口'; then
    ok jaccesswalker "$(printf '%s' "$out" | head1)"; else bad jaccesswalker "$(printf '%s' "$out" | head1)"; fi

# jconsole：真连上目标并画出窗口，再用 jaccessinspector 读它的控件树（连"窗口起来了但没数据"都能抓到）
( cd "$W" && "$BIN/jconsole.exe" -p "$PID" > jconsole.log 2>&1 & echo $! > jc.pid )
PIDS+=("$(cat "$W/jc.pid")")
JCWIN=""
for _ in $(seq 1 40); do
    JCWIN=$(ps -W 2>/dev/null | grep -i '[j]console' | awk '{print $4}' | head -1)
    [ -n "$JCWIN" ] && break
    sleep 0.25
done
if [ -n "$JCWIN" ] && out=$(timeout 40 "$BIN/jaccessinspector.exe" -pid "$JCWIN" -o tree -d 3 2>&1) \
   && printf '%s' "$out" | grep -q '已连接'; then
    ok jconsole "$(printf '%s' "$out" | grep '已连接' | head1)"
    ok jaccessinspector "$(printf '%s' "$out" | grep 'role=' | head1)"
else
    bad jconsole "进程 $( [ -n "$JCWIN" ] && echo "$JCWIN" || echo 未起 ) ${out:-无输出}"
    bad jaccessinspector "$(printf '%s' "${out:-}" | head1)"
fi

# ---------------------------------------------------------------- 汇总
echo "---------------------------------------------------------------"
echo "通过 $PASSED / 失败 $FAILED"
[ "$FAILED" -eq 0 ] || exit 1
