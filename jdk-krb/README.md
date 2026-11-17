# vmless-krb — Kerberos v5 的最小正确子集

自己实现的一套 Kerberos v5：ASN.1 报文、RFC 3962 的 AES-CTS 加密、MIT `FILE:` 凭据缓存、
`kinit` / `klist`，外加一个**能跟真 JDK 互通**的最小 KDC。

只依赖 `vmless-crypto`（DER 原语、AES、SHA-1/HMAC/PBKDF2、随机源），没有第三方库。

## 能做什么

```bash
# 1) 造 keytab（用 jdk-crypto 的 ktab；也可以直接用 --add 走口令）
ktab -a "krbtgt/VMKRB.LOCAL@VMKRB.LOCAL" -k krb5.keytab -p krbtgt-pw -e aes256-cts
ktab -a "testuser@VMKRB.LOCAL"            -k krb5.keytab -p secretpw  -e aes256-cts
ktab -a "host/localhost@VMKRB.LOCAL"      -k krb5.keytab -p svc-pw    -e aes256-cts

# 2) 起 KDC（UDP + TCP 同一个端口）
kdc --realm VMKRB.LOCAL --port 88 --keytab krb5.keytab --verbose
#    或者不用 keytab，直接按口令现算长期密钥：
kdc --realm VMKRB.LOCAL --port 88 --add krbtgt/VMKRB.LOCAL:krbtgt-pw --add testuser:secretpw

# 3) 登录
kinit testuser@VMKRB.LOCAL --realm VMKRB.LOCAL --kdc 127.0.0.1:88 -c FILE:./cc
kinit --password secretpw --tgs host/localhost testuser@VMKRB.LOCAL --realm VMKRB.LOCAL --kdc 127.0.0.1:88 -c FILE:./cc
kinit -t krb5.keytab testuser@VMKRB.LOCAL --realm VMKRB.LOCAL --kdc 127.0.0.1:88 -c FILE:./cc

# 4) 看票
klist -c FILE:./cc -e -f

# 5) fuzz（三个解析入口，各 N 例）
krbfuzz --iters 50000
```

`KRB5CCNAME` / `KRB5_REALM` / `KRB5_KDC` / `KRB5_PRINCIPAL` 都认。

## 互通验收（`bash e2e.sh`，14 项全绿）

| 判据 | 结果 |
| --- | --- |
| 我们的 `kinit` → 我们的 KDC → 我们的 `klist` | 通过 |
| **真 JDK `kinit` → 我们的 KDC** | 通过 |
| 我们的 ccache → **真 JDK `klist`** | 通过 |
| `ktab` 写出的 keytab → 我们的 KDC 验预认证 | 通过 |
| 错口令 / 未知 principal 的错误码与文案（双方） | 通过 |
| TGS-REQ/TGS-REP 换服务票（JDK klist 能看到 2 条） | 通过 |
| TCP 传输、KDC 不在、坏缓存 | 通过 |

## 模块

| 文件 | 内容 |
| --- | --- |
| `src/asn1_krb.rs` | RFC 4120 的 ASN.1 类型（`EXPLICIT TAGS` + `[APPLICATION n]`） |
| `src/crypto_krb.rs` | n-fold / DR / DK / string2key / AES-CTS(CS3) / HMAC-SHA1-96 |
| `src/msg.rs` | key usage 编号、预认证、KRB-ERROR、报文构造 |
| `src/ccache.rs` | MIT `FILE:` 凭据缓存（0x0504 写、0x0503/0x0504 读） |
| `src/client.rs` | `kinit` / `klist` / TGS 客户端、UDP+TCP 传输 |
| `src/kdc.rs` | keytab（JDK 与 MIT 两种格式）、KDB、AS/TGS 处理、服务循环 |
| `src/bin/{kinit,klist,kdc,krbfuzz}.rs` | 四个可执行 |

## 几个关键决定（为什么这么写）

- **CTS 用 CS3**（总是交换最后两个密文块），与 JDK 的 `CipherTextStealing` 逐行对齐。
  长度正好是 16 的整数倍时也交换 —— 这一条最容易写错，测试里用 JDK 的真实输出钉死了
  `len=16/17/31/32/33/47/48/64` 八种情形。
- **string2key 必须带 `DK(tkey, "kerberos")`**（RFC 3962 §4）。少了这一步，密钥与
  JDK/MIT 全部对不上。RFC 3962 附录 B 与 JDK 25 的 `Aes256.stringToKey` 两组向量都验证过。
- **key usage 用 RFC 4120 §7.5.1 的编号**：票是 2、AS-REP enc-part 是 3、TGS-REQ
  authenticator 是 7、TGS-REP enc-part 是 8、AP-REQ authenticator 是 11。写成 2/4/5/6
  的话自己跟自己能对上，但跟任何真实现都对不上。
- **ccache 的 `is_skey` 是 1 个字节**、keyblock 长度是 32 位、v4 header 是「整个 header
  的字节长度」。三条都照 krb5 的 `doc/formats/ccache_file_format.rst` 来。
- **KDC 的 `e_text` 只用 ASCII**：这段字符串会原样出现在 JDK 的报错里。

## 已知限制

- 只实现 `aes256-cts-hmac-sha1-96`(18) 与 `aes128-cts-hmac-sha1-96`(17)；
  `rc4-hmac`(23)、`des3`、AES-SHA2(19/20) 会明确报「不支持的加密类型」而不是静默降级。
- 不支持跨 realm 的 referral（`KDC_ERR_WRONG_REALM` 直接拒）。
- 不实现 PAC、不实现 `kadmin` 协议；KDC 的数据库来自 keytab 或 `--add`，重启即重建。
- 没有 replay cache（TGS 的 authenticator 只校验时钟偏移，不做重放检测）。
- `ktab`（jdk-crypto）当前写出的密钥**少一步 `DK(tkey,"kerberos")`**，所以
  「ktab 写的 keytab + 标准口令客户端」不匹配；本实现的 KDC 与 `kinit -t` 用同一份
  keytab 时不受影响。修法见提交说明。
