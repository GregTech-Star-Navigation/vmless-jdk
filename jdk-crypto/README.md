# vmless-crypto

零依赖（`rustjavac` 为可选依赖，本栈不引用其任何类型）的密码学栈：摘要 / HMAC / 大数 / RSA / AES / ASN.1 / X.509 /
PKCS#7 / PKCS#12 / JAR 签名，附带三个能真用的命令行工具 —— `keytool`、`jarsigner`、`ktab`。
目标是**与真 JDK 双向互操作**：我们签的 JAR 能被真 `jarsigner` 验过，真 `jarsigner` 签的也能被我们验过；
`.p12` 与证书同理。

状态：0.1.0（2026-11-17）。全部算法自实现，未引入任何第三方 crate。

`cargo build` 不需要 `rustjavac` 编译通过（它是 optional feature `classfile`）；
运行时的 `JarFile` 接 `jar_sign::verify_jar(&[u8]) -> Result<VerifyReport>` 即可。

## 快速开始

```bash
cargo build --release --bins

# 造一个密钥库（PKCS12，自签证书，默认 SHA384withRSA，与 JDK 25 一致）
./target/release/keytool -genkeypair -keystore my.p12 -storepass changeit \
    -alias mykey -keysize 2048 -dname "CN=Me,O=Example,C=CN" -validity 365

# 签名 + 验签
./target/release/jarsigner -keystore my.p12 -storepass changeit app.jar mykey
./target/release/jarsigner -verify app.jar      # jar 已验证。

# keytab
./target/release/ktab -a user/admin@EXAMPLE.COM -k test.keytab -p secret
./target/release/ktab -l -k test.keytab
```

## 模块

| 文件 | 内容 |
| --- | --- |
| `src/hash/{sha1,sha256,sha512,md5,hmac}.rs` | 摘要、HMAC、PBKDF2、PKCS#12 KDF；常数时间比较 `ct_eq` |
| `src/bigint.rs` | `u32` limb 大数：加减乘、Knuth-D 除法、Montgomery 模幂、模逆、Miller-Rabin、素数生成 |
| `src/rsa.rs` | 密钥生成（CRT）、PKCS#1 v1.5 签名/验签（严格按 RFC 8017 §8.2.2）、PKCS#1/PKCS#8 编解码 |
| `src/cipher.rs` | AES-128/192/256（S 盒由 GF(2^8) 求逆算出）+ CBC + PKCS#7 填充 |
| `src/asn1/{der,oid,types}.rs` | 严格 DER 解析（拒绝不定长/非最小长度/截断）、编码、`Name`、`AlgorithmIdentifier` |
| `src/x509.rs` | 证书解析（版本/序列号/签名算法/有效期/扩展）、自签生成、自校验 |
| `src/pkcs7.rs` | SignedData 生成（分离内容 + 签名属性）与验证 |
| `src/pkcs12.rs` | `.p12` 读写：AuthenticatedSafe + MAC、PBES2/PBKDF2/AES-CBC、三种 bag |
| `src/zip.rs` | ZIP 读写 + raw DEFLATE 解码 + CRC-32 |
| `src/jar_sign.rs` | MANIFEST.MF / .SF / .RSA 生成与逐条目校验 |
| `src/bin/{keytool,jarsigner,ktab}.rs` | 命令行入口 |
| `src/bin/ctool.rs` | 交叉验证 / 压测 / fuzz（不进交付面） |

## 安全设计要点

- **常数时间**：`hash::ct_eq` 用于 HMAC 校验、PKCS#7 `messageDigest` 比对、PKCS#12 MAC、
  JAR 条目摘要比对、RSA 验签里的 DigestInfo 比较（填充里非 0xFF 的检查用累积变量而非提前退出）。
  **不是**常数时间的：RSA 私钥运算（`modpow` 用 Montgomery，平方乘按指数位分支）、
  PKCS#1 v1.5 **签名**（填充合法，不存在 Bleichenbacher 式自适应选择）、AES 软件实现（无 AES-NI，
  查表访问 S 盒，存在缓存计时侧信道）。这些都要靠"口令/私钥本机可信"这一前提。
- **严格填充**：RSA 验签要求 `00 01 FF..FF 00 || DigestInfo`、PS ≥ 8、DigestInfo 逐字节相同；
  不做"只比尾部哈希"。
- **严格 DER**：拒绝 BER 不定长、非最小长度编码、冗余前导 0 的 INTEGER、越界长度；
  任何畸形输入只返回 `Err`。
- **口令错误有明确话术**：PKCS#12 MAC 不匹配 → 「口令错误或文件损坏：PKCS#12 的完整性校验（MAC）不匹配」。

## 已知边界

- 只做 RSA（jarsigner 的 `*.RSA`；不生成 `*.DSA`/`*.EC`）。
- 一次只验一个签名块（多签名者会明确报错）。
- 没有时间戳（TSA）支持；验签会提示"没有时间戳"。
- 不支持 ZIP64（>4GB 或 >65535 条目直接报错）。
- `.p12` 只支持 PBES2/AES-CBC；旧式 `PBEWithSHA1AndDESede`/`RC2_40` 会给出"请用 keytool -importkeystore 升级"的提示。
- `keytool` 只实现 `-list/-genkeypair/-exportcert/-importcert/-printcert/-delete`。

## 验收（2026-11-17 实测）

| # | 项目 | 命令与结果 |
| --- | --- | --- |
| 1 | 摘要 4 算法 × 3 数据 | `ctool hash sha256 blob1m.bin` 与 `sha256sum` 逐字节一致（12/12 OK） |
| 2 | 我们签 → 真 jarsigner | `jarsigner -verify our.jar` → `jar 已验证`，exit 0 |
| 3 | 真 jarsigner 签 → 我们验 | `jarsigner -verify t.jar` → `jar 已验证。`，exit 0（含 SHA-1/SHA-256 两种摘要） |
| 4 | 证书 | `keytool -printcert -file ourcert.der` 读出 Owner/Subject/序列号/有效期/SHA384withRSA/2048-bit/v3 |
| 5 | PKCS#12 双向 | 真 keytool 读我们的 p12（PrivateKeyEntry / trustedCertEntry）；我们读真 keytool 的 p12（SHA-256 指纹逐字符相同） |
| 6 | JDK 自带签名 jar | JDK 里没有签名 jar（`sunec.jar`、`jrt-fs.jar` 均未签名），由 #2/#3 的双向签名覆盖 |
| 7 | keytab | `ktab -l` 与 JDK `klist -k -e` 都能解析（2 entries，KVNO 3/1，key type 18/17） |

其它：`cargo test --lib` 62 项全绿；`ctool fuzz 100000`（der/x509/p12 各 10 万次，含结构化变异）0 panic；
RSA-2048 签名 15.2ms / 验签 3.6ms；SHA-256 144 MB/s；PKCS#12 加载 75ms（10k 次 PBKDF2 ×2 + MAC）。
