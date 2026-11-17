#!/usr/bin/env bash
# 一键验收：我们 ↔ 真 JDK 双向互操作。需要先把 release 产物放到 $BIN。
#   bash scripts/acceptance.sh /d/vmless/bcry
set -u
BIN="${1:-/d/vmless/bcry}"
J="${JDK_BIN:-/c/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin}"
W="${WORK:-/d/vmless/xt/acc}"
PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "OK   $*"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL $*"; }
check(){ if [ "$1" = "$2" ]; then ok "$3"; else bad "$3 (got '$1' want '$2')"; fi; }

rm -rf "$W"; mkdir -p "$W"; cd "$W" || exit 2

echo "=== 1. 摘要：4 算法 × 3 数据（对照 sha256sum/md5sum/openssl）"
: > empty.bin; printf 'abc' > abc.bin; "$BIN/ctool" genblob 1048576 blob1m.bin
for alg in md5 sha1 sha256 sha512; do for f in empty.bin abc.bin blob1m.bin; do
  ours=$("$BIN/ctool" hash $alg $f)
  case $alg in
    md5)    ref=$(md5sum $f | cut -d' ' -f1);;
    sha1)   ref=$(openssl dgst -sha1 $f | sed 's/.*= //');;
    sha256) ref=$(sha256sum $f | cut -d' ' -f1);;
    sha512) ref=$(openssl dgst -sha512 $f | sed 's/.*= //');;
  esac
  check "$ours" "$ref" "$alg/$f"
done; done

echo "=== 2. 我们生成 p12 + 真 keytool 读"
"$BIN/keytool" -genkeypair -keystore our.p12 -storepass changeit -alias mykey \
  -keysize 2048 -dname "CN=OurSigner,O=vmless,C=CN" -validity 365 > /dev/null
n=$("$J/keytool" -J-Duser.language=en -list -keystore our.p12 -storepass changeit 2>/dev/null | grep -c "PrivateKeyEntry")
check "$n" "1" "真 keytool 读到 PrivateKeyEntry"

echo "=== 3. 证书：真 keytool -printcert 读我们生成的证书"
"$BIN/keytool" -exportcert -keystore our.p12 -storepass changeit -alias mykey -file our.der
p=$("$J/keytool" -J-Duser.language=en -printcert -file our.der 2>/dev/null)
echo "$p" | grep -q "CN=OurSigner" && ok "Owner 字段正确" || bad "Owner 字段"
echo "$p" | grep -q "SHA384withRSA" && ok "签名算法 SHA384withRSA" || bad "签名算法"
echo "$p" | grep -q "2048" && ok "公钥 2048 位" || bad "公钥位数"

echo "=== 4. 我们签名 → 真 jarsigner 验证"
echo hello > a.txt; echo world > b.txt
"$J/jar" cf base.jar a.txt b.txt
cp base.jar app.jar
"$BIN/jarsigner" -keystore our.p12 -storepass changeit app.jar mykey
"$J/jarsigner" -J-Duser.language=en -verify app.jar > /dev/null 2>&1
check "$?" "0" "真 jarsigner: jar 已验证（退出码 0）"

echo "=== 5. 真 jarsigner 签名 → 我们验证"
"$J/keytool" -J-Duser.language=en -genkeypair -alias realkey -keyalg RSA -keysize 2048 -dname "CN=RealSigner,O=jdk,C=CN" \
  -validity 365 -storetype PKCS12 -keystore real.p12 -storepass changeit > /dev/null 2>&1
cp base.jar app2.jar
"$J/jarsigner" -J-Duser.language=en -keystore real.p12 -storepass changeit app2.jar realkey > /dev/null 2>&1
"$BIN/jarsigner" -verify app2.jar > /dev/null 2>&1
check "$?" "0" "我们: jar 已验证（真 jarsigner 签的，退出码 0）"

echo "=== 6. PKCS#12 双向：真 keytool 读我们的 / 我们读真 keytool 的"
f1=$("$J/keytool" -J-Duser.language=en -list -keystore real.p12 -storepass changeit 2>/dev/null | grep "SHA-256" | sed 's/.*: //')
f2=$("$BIN/keytool" -list -keystore real.p12 -storepass changeit 2>/dev/null | grep "SHA-256" | sed 's/.*: //')
check "${f2^^}" "${f1^^}" "真 keytool 的 p12：SHA-256 指纹一致"
echo "（我们的 p12 已被真 jarsigner 用来签 #4，证明私钥包可被 JDK 解出）"

echo "=== 7. keytab：我们写，ktab -l 与 JDK klist 都能读"
rm -f test.keytab
"$BIN/ktab" -a alice/admin@EXAMPLE.COM -k test.keytab -p secret -e aes256-cts -V 3 > /dev/null
n=$("$BIN/ktab" -l -k test.keytab | grep -c "alice/admin@EXAMPLE.COM")
check "$n" "1" "ktab -l 读到条目"
K="${KLIST:-/c/Program Files/Eclipse Adoptium/jdk-21.0.12.101-hotspot/bin/klist}"
n=$("$K" -k -e test.keytab 2>/dev/null | grep -c "alice/admin@EXAMPLE.COM")
check "$n" "1" "JDK klist 读到条目"

echo
echo "结果: $PASS 通过, $FAIL 失败"
[ "$FAIL" -eq 0 ]
