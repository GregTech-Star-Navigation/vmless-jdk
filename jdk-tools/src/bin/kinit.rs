//! `kinit` —— 本 crate 不实现的那一部分（能力归属见下方 `MISSING`/`SUBSYSTEM`）。
//!
//! 这是**有明确归属的能力缺口**，不是占位实现：工具会打印缺什么、归哪个子系统、
//! 现在能用什么替代，然后以退出码 1 结束（与真 JDK 出错时的退出码一致）。

const TOOL: &str = "kinit";
const MISSING: &str = "Kerberos AS-REQ/AS-REP 交互、票据加密（AES/DES）、ccache 写入";
const SUBSYSTEM: &str = "kerberos";
const ALT: &str = "用系统 MIT Kerberos 的 kinit，或 JDK 的 kinit";

fn main() {
    vmless_tools::install_pipe_guard();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| matches!(a.as_str(), "-h" | "--help" | "-?" | "-help")) {
        println!("{TOOL}: 本 crate 不提供的工具");
        println!("缺少的能力：{MISSING}");
        println!("归属子系统：{SUBSYSTEM}");
        println!("可用替代：{ALT}");
        println!();
        println!("这个 exe 存在是为了让 JDK 的工具集完整、让脚本拿到明确原因；");
        println!("不带参数运行会打印同样的说明并以退出码 1 结束。");
        return;
    }
    vmless_tools::capability_gap(TOOL, MISSING, SUBSYSTEM, ALT);
}
