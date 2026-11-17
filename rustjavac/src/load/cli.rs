//! `rustjava` 的 JPMS 参数解析（`--module-path` / `--add-opens` / `--describe-module` …）。
//!
//! 只做**纯解析**，不碰文件系统：解析结果是一份 `JpmsOptions`，由 `vm::jpms` 去执行。
//! 支持的写法与真 java 一致：`--opt value` 与 `--opt=value` 都认，`-p` / `-m` / `-cp` 是短别名。
//!
//! 解析顺序：参数按出现顺序逐个吃，**同一个 `--add-opens` 出现多次时后一条覆盖前一条**
//! （键是 `模块/包=目标`）；不同键之间是并集 —— 与真 java 的 `ModuleLayer` 规则一致。

/// 一条 `--add-opens` / `--add-exports`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleRule {
    pub module: String,
    pub package: String,
    pub target: String,
}

/// 一条 `--add-reads`
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadRule {
    pub module: String,
    pub target: String,
}

#[derive(Clone, Debug, Default)]
pub struct JpmsOptions {
    pub module_path: Vec<String>,
    pub class_path: Option<String>,
    pub add_modules: Vec<String>,
    pub limit_modules: Vec<String>,
    pub add_opens: Vec<ModuleRule>,
    pub add_exports: Vec<ModuleRule>,
    pub add_reads: Vec<ReadRule>,
    /// `--describe-module <名>`
    pub describe_module: Option<String>,
    /// `--print-module-resolution [根模块]`
    pub print_resolution: Option<Option<String>>,
    /// `--module <m>[/<主类>]`
    pub launch_module: Option<(String, Option<String>)>,
    /// 剩下的位置参数（主类 / class 文件 / jar）
    pub rest: Vec<String>,
}

impl JpmsOptions {
    /// 有没有用到任何模块相关开关（决定要不要进模块模式）
    pub fn wants_modules(&self) -> bool {
        !self.module_path.is_empty()
            || !self.add_modules.is_empty()
            || !self.limit_modules.is_empty()
            || !self.add_opens.is_empty()
            || !self.add_exports.is_empty()
            || !self.add_reads.is_empty()
            || self.describe_module.is_some()
            || self.print_resolution.is_some()
            || self.launch_module.is_some()
    }

    /// 把 `--add-opens` 按 `模块/包=目标` 去重（后面的覆盖前面的，保留最后一次出现的顺序）
    fn dedupe_rules(list: Vec<ModuleRule>) -> Vec<ModuleRule> {
        let mut out: Vec<ModuleRule> = Vec::with_capacity(list.len());
        for r in list {
            if let Some(slot) = out
                .iter_mut()
                .find(|x| x.module == r.module && x.package == r.package && x.target == r.target)
            {
                *slot = r;
            } else {
                out.push(r);
            }
        }
        out
    }

    pub fn normalized(mut self) -> Self {
        self.add_opens = Self::dedupe_rules(self.add_opens);
        self.add_exports = Self::dedupe_rules(self.add_exports);
        self
    }
}

/// 这个参数名是不是 JPMS 开关（`rustjava` 用它决定要不要走模块通道）
///
/// **`-cp` / `--class-path` 故意不算**：它们单独出现时仍然是旧的扁平 classpath，
/// 只有和真正的模块开关一起出现才意味着「模块模式 + 用户 classpath」。
pub fn is_jpms_flag(arg: &str) -> bool {
    let name = arg.split('=').next().unwrap_or(arg);
    matches!(
        name,
        "--module-path"
            | "-p"
            | "--add-modules"
            | "--limit-modules"
            | "--add-opens"
            | "--add-exports"
            | "--add-reads"
            | "--module"
            | "-m"
            | "--describe-module"
            | "--print-module-resolution"
            | "--show-module-resolution"
            | "--module-version"
            | "--list-modules"
    )
}

fn split_eq(arg: &str) -> (&str, Option<&str>) {
    match arg.split_once('=') {
        Some((a, b)) => (a, Some(b)),
        None => (arg, None),
    }
}

/// `m/p=target` → `ModuleRule`
fn parse_rule(spec: &str, what: &str) -> Result<ModuleRule, String> {
    let (left, target) = spec
        .split_once('=')
        .ok_or_else(|| format!("{what} 的格式是 <模块>/<包>=<目标>，实际是 {spec:?}"))?;
    let (module, package) = left
        .split_once('/')
        .ok_or_else(|| format!("{what} 的格式是 <模块>/<包>=<目标>，实际是 {spec:?}"))?;
    if module.is_empty() || package.is_empty() || target.is_empty() {
        return Err(format!("{what} 里有空字段: {spec:?}"));
    }
    Ok(ModuleRule {
        module: module.to_string(),
        package: package.replace('/', "."),
        target: target.to_string(),
    })
}

/// `m=target` → `ReadRule`
fn parse_read(spec: &str) -> Result<ReadRule, String> {
    let (module, target) = spec
        .split_once('=')
        .ok_or_else(|| format!("--add-reads 的格式是 <模块>=<目标>，实际是 {spec:?}"))?;
    Ok(ReadRule { module: module.to_string(), target: target.to_string() })
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// 解析一整个命令行（已经去掉 `-D` 之后的参数）
pub fn parse(args: &[String]) -> Result<JpmsOptions, String> {
    let mut o = JpmsOptions::default();
    let mut i = 0usize;
    let need = |args: &[String], i: usize, what: &str| -> Result<String, String> {
        args.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{what} 后面要跟一个值"))
    };
    while i < args.len() {
        let a = args[i].as_str();
        let (name, inline) = split_eq(a);
        match name {
            "--module-path" | "-p" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.module_path.extend(v.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
            "--class-path" | "-cp" | "-classpath" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.class_path = Some(v);
            }
            "--add-modules" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.add_modules.extend(split_list(&v));
            }
            "--limit-modules" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.limit_modules.extend(split_list(&v));
            }
            "--add-opens" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.add_opens.push(parse_rule(&v, "--add-opens")?);
            }
            "--add-exports" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.add_exports.push(parse_rule(&v, "--add-exports")?);
            }
            "--add-reads" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.add_reads.push(parse_read(&v)?);
            }
            "--module" | "-m" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                let (m, main) = match v.split_once('/') {
                    Some((m, mc)) => (m.to_string(), Some(mc.to_string())),
                    None => (v.clone(), None),
                };
                if m.is_empty() {
                    return Err(format!("--module 里没有模块名: {v:?}"));
                }
                o.launch_module = Some((m, main));
            }
            "--describe-module" => {
                let v = match inline {
                    Some(v) => v.to_string(),
                    None => {
                        i += 1;
                        need(args, i - 1, name)?
                    }
                };
                o.describe_module = Some(v);
            }
            "--print-module-resolution" | "--show-module-resolution" => {
                // 值是可选的：下一个参数是开关就当作没有根模块
                let root = match inline {
                    Some(v) => Some(v.to_string()),
                    None => args
                        .get(i + 1)
                        .filter(|n| !n.starts_with('-'))
                        .map(|n| n.to_string()),
                };
                if inline.is_none() && root.is_some() {
                    i += 1;
                }
                o.print_resolution = Some(root);
            }
            "--list-modules" => {
                o.print_resolution = Some(None);
            }
            _ => o.rest.push(a.to_string()),
        }
        i += 1;
    }
    Ok(o.normalized())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_module_path_short_and_long() {
        let o = parse(&v(&["-p", "a;b", "--module-path=c", "Main"])).unwrap();
        assert_eq!(o.module_path, vec!["a", "b", "c"]);
        assert_eq!(o.rest, vec!["Main"]);
    }

    #[test]
    fn add_opens_repeatable_last_wins() {
        let o = parse(&v(&[
            "--add-opens",
            "java.base/java.lang.invoke=ALL-UNNAMED",
            "--add-opens",
            "java.base/java.lang.invoke=ALL-UNNAMED",
            "--add-opens=java.base/java.lang=ALL-UNNAMED",
        ]))
        .unwrap();
        assert_eq!(o.add_opens.len(), 2);
        assert_eq!(o.add_opens[0].package, "java.lang.invoke");
        assert_eq!(o.add_opens[1].target, "ALL-UNNAMED");
    }

    #[test]
    fn module_launch_parsed() {
        let o = parse(&v(&["--module", "app/com.example.Main"])).unwrap();
        assert_eq!(
            o.launch_module,
            Some(("app".to_string(), Some("com.example.Main".to_string())))
        );
    }

    #[test]
    fn bad_rule_reports() {
        let e = parse(&v(&["--add-opens", "java.base"])) .unwrap_err();
        assert!(e.contains("格式"), "{e}");
    }

    #[test]
    fn describe_module_flag() {
        let o = parse(&v(&["--describe-module", "java.base"])).unwrap();
        assert_eq!(o.describe_module.as_deref(), Some("java.base"));
        assert!(o.wants_modules());
    }
}
