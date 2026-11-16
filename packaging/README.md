# packaging —— 可分发的 Windows 安装包

> 状态：已跑通（2026-11-17，本地实测见本节末尾）· 产物不提交进 git（`dist/` 在 .gitignore 里）

一条命令重打三件产物：

```bash
packaging/build.sh                 # cargo build + 三种产物
packaging/build.sh -SkipBuild      # 只重打包（用 target/release 里现成的 exe）
```

Windows 下等价：`pwsh -File packaging\build.ps1`。

## 产物（`dist/`）

| 文件 | 说明 |
|---|---|
| `vmless-<ver>-win-x64.zip` | 便携包：`bin\{rustjava,compile_src,run_jit}.exe` + `README.txt` + `LICENSE`，解压即用 |
| `vmless-<ver>-setup.exe` | NSIS 安装包：装到 `%LOCALAPPDATA%\vmless`，可选加入用户 PATH，写卸载项，带 `uninstall.exe` |
| `SHA256SUMS` | 上面两个文件的 sha256（`hash  文件名`，两空格，与 `sha256sum -c` 同格式） |
| `release-notes.md` | Release 正文：从 `CHANGELOG.md` 抽版本对应小节 |

版本号**不手写**：读 `rustjavac/Cargo.toml` 的 `[package] version`。
若 `-Version` 与 Cargo.toml 不一致，脚本会黄字提示；若 Cargo.toml 与 CHANGELOG 最新一节不一致，
打包照常但会警告（release notes 里也会写明取的是哪一节）。

## 依赖

- **cargo**（Rust 1.100 实测）：`cargo build --release --bin rustjava` 与
  `--example compile_src --example run_jit`（后两个是 examples，产物在 `target/release/examples/`）
- **NSIS 3.x**（只在打 setup.exe 时需要）：装一份，或解压官方 zip 后用 `-Makensis` 指过去。
  没找到时脚本不报错，只生成 zip + 校验和，并打一条黄字警告。
- PowerShell 5.1 即可（`Compress-Archive` / `Get-FileHash` 都是内置的）

## 安装包行为

- 装到 `%LOCALAPPDATA%\vmless`，**不需要管理员权限**（`RequestExecutionLevel user`）
- 组件：`运行时（必需）` + `把 bin 加入当前用户 PATH`（默认勾选，卸载时移除）
- 卸载：`uninstall.exe`，清文件、清 `HKCU\Software\vmless`、清卸载项、删 PATH 条目
- 静默安装：`vmless-<ver>-setup.exe /S`（PATH 组件按默认勾选状态处理）

## 注意

- **`LICENSE` 目前是占位文本**（`packaging/LICENSE.placeholder`）。分发包必须有 LICENSE，
  但项目还没定许可证 —— 定了以后放到仓库根 `LICENSE`，脚本会优先用它。
- 便携包里**不含 JDK 布局夹具**（`vmless-jdk25/`，259 MB）。那套是另一条线（`bin\javac.exe`），
  与这三个 exe 不是同一个交付物。
- 打出来的 exe 没有代码签名，SmartScreen 会拦一下（「更多信息 → 仍要运行」）。

## 实测记录

2026-11-17，本机（Windows + Rust 1.100 + NSIS 3.10 解压版）实际跑出来的一次：

| 产物 | 体积 | sha256 |
|---|---|---|
| `vmless-0.1.0-win-x64.zip` | 1,613,331 B（1.54 MB） | `43eaa0537450a2ebd39ac9f983747de48a89ae73366fca2bafdded8ba84ce007` |
| `vmless-0.1.0-setup.exe` | 1,190,405 B（1.14 MB） | `93859416c5569c3a45dec332d1b09705bde9955a050db001f9fa3260ed686bb3` |
| `SHA256SUMS` | 180 B | — |
| `release-notes.md` | 1,877 B | — |

`sha256sum -c SHA256SUMS` 两个文件都 `OK`。

zip 内容（`7z l` 的实际输出，5 个文件 / 解压后 4,098,747 B）：

```
bin/compile_src.exe   977,920
bin/run_jit.exe     2,766,848
bin/rustjava.exe      351,744
LICENSE                   537
README.txt              1,698
```

安装包实测（`vmless-0.1.0-setup.exe /S /D=<临时目录>`）：

1. 装完 `bin/` 下三个 exe 都在，`uninstall.exe` 也在；
2. 用装好的三个程序编译并运行一个 Hello（解释器与 JIT 都输出 `installed ok`）；
3. `uninstall.exe /S` 之后安装目录整个消失（含卸载器自己），
   `HKCU\Environment` 的 `Path` 回到安装前的字符串，**值的类型仍是 `REG_EXPAND_SZ`**
   （`%USERPROFILE%` 这类变量没有被写成字面量 —— 这个坑是 `WriteRegStr` 造成的，已改成 `WriteRegExpandStr`）。

版本口径提醒：这次打出来的包叫 `0.1.0`，因为 `rustjavac/Cargo.toml` 是 `0.1.0`
（`CHANGELOG.md` 已经记到 `0.42.0`）。脚本按 `Cargo.toml` 取版本，两者不一致时打黄字警告，
`release-notes.md` 里也会写一句「取的是最新一节」。

