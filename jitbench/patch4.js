const fs = require("fs");
const p = "src/jit/inline.rs";
let s = fs.readFileSync(p, "utf8");

s = s.replace(
  "/// 把一个方法体里能内联的调用点就地展开。返回 (新字节码, 新 max_locals, 内联了几个点)。",
  "/// 把一个方法体里能内联的调用点就地展开。\n/// 返回 (新字节码, 新 max_locals, 额外需要的 max_stack, 内联了几个点)。"
);
s = s.replace(") -> (Vec<u8>, usize, usize) {", ") -> (Vec<u8>, usize, usize, usize) {");
s = s.replace(/return \(body\.to_vec\(\), max_locals, 0\);/g, "return (body.to_vec(), max_locals, 0, 0);");
s = s.replace(
  "    let mut sites: Vec<(usize, Vec<u8>, usize)> = Vec::new(); // (调用点 pc, 尾巴, 用掉的槽数)",
  "    let mut sites: Vec<(usize, Vec<u8>, usize, usize)> = Vec::new(); // (调用点 pc, 尾巴, 槽数, 栈深)"
);
s = s.replace(
  "    let mut slots = max_locals;\n    let mut pc = 0usize;",
  "    let mut slots = max_locals;\n    let mut stack_extra = 0usize;\n    let mut pc = 0usize;"
);
s = s.replace(
  `            if let Some((tail, used)) =
                build_tail(classes, cf, idx, op == 0xb6, slots, MAX_DEPTH, &mut visited)
            {
                sites.push((pc, tail, used));
                slots += used;
            }`,
  `            if let Some((tail, used, deep)) =
                build_tail(classes, cf, idx, op == 0xb6, slots, MAX_DEPTH, &mut visited)
            {
                sites.push((pc, tail, used, deep));
                slots += used;
                stack_extra = stack_extra.max(deep);
            }`
);
s = s.replace("    for (_, tail, _) in &sites {", "    for (_, tail, _, _) in &sites {");
s = s.replace("    for (k, (call_pc, _, _)) in sites.iter().enumerate() {", "    for (k, (call_pc, _, _, _)) in sites.iter().enumerate() {");
s = s.replace("    for (k, (call_pc, tail, _)) in sites.iter().enumerate() {", "    for (k, (call_pc, tail, _, _)) in sites.iter().enumerate() {");
s = s.replace("    let n = sites.len();\n    (out, slots, n)", "    let n = sites.len();\n    (out, slots, stack_extra, n)");

// build_tail / emit_call 增加 max_stack 透传
s = s.replace(
  `fn build_tail(
    classes: &ClassSet,
    cf: &ClassFile,
    idx: u16,
    virtual_call: bool,
    base: usize,
    depth: u32,
    visited: &mut HashSet<Key>,
) -> Option<(Vec<u8>, usize)> {
    let (tail, used, _) = emit_call(classes, cf, idx, virtual_call, base, depth, visited, true)?;
    Some((tail, used))
}`,
  `fn build_tail(
    classes: &ClassSet,
    cf: &ClassFile,
    idx: u16,
    virtual_call: bool,
    base: usize,
    depth: u32,
    visited: &mut HashSet<Key>,
) -> Option<(Vec<u8>, usize, usize)> {
    let (tail, used, deep, _) = emit_call(classes, cf, idx, virtual_call, base, depth, visited, true)?;
    Some((tail, used, deep))
}`
);
s = s.replace(
  ") -> Option<(Vec<u8>, usize, usize)> {\n    let (cls, name, cdesc) = method_ref_of(cf, idx).ok()?;",
  ") -> Option<(Vec<u8>, usize, usize, usize)> {\n    let (cls, name, cdesc) = method_ref_of(cf, idx).ok()?;"
);
s = s.replace(
  "    let (region, used) = build_region(\n        classes,\n        &callee.cf,\n        &callee.body,\n        &key,\n        base,\n        callee.max_locals,\n        depth - 1,\n        &mut deeper,\n    )?;",
  "    let (region, used, deep) = build_region(\n        classes,\n        &callee.cf,\n        &callee.body,\n        &key,\n        base,\n        callee.max_locals,\n        callee.max_stack,\n        depth - 1,\n        &mut deeper,\n    )?;"
);
s = s.replace(
  "    let at = bytes.len();\n    if trailing_goto {\n        bytes.extend_from_slice(&[0xa7, 0, 0]);\n    }\n    Some((bytes, used, at))",
  "    let at = bytes.len();\n    if trailing_goto {\n        bytes.extend_from_slice(&[0xa7, 0, 0]);\n    }\n    Some((bytes, used, deep, at))"
);

// build_region 增加 stack_in / 返回 max_stack
s = s.replace(
  "    base: usize,\n    slots_in: usize,\n    depth: u32,\n    visited: &mut HashSet<Key>,\n) -> Option<(Vec<u8>, usize)> {",
  "    base: usize,\n    slots_in: usize,\n    stack_in: usize,\n    depth: u32,\n    visited: &mut HashSet<Key>,\n) -> Option<(Vec<u8>, usize, usize)> {"
);
s = s.replace(
  "    let mut slots = slots_in;\n    let mut pc = 0usize;\n    while pc < body.len() {\n        let op = body[pc];",
  "    let mut slots = slots_in;\n    let mut stack_used = stack_in;\n    let mut pc = 0usize;\n    while pc < body.len() {\n        let op = body[pc];"
);
s = s.replace(
  "                let (bytes, used, _) = emit_call(",
  "                let (bytes, used, deep, _) = emit_call("
);
s = s.replace(
  "                out.extend_from_slice(&bytes);\n                // 嵌套调用各占一段槽（保守：不共用，反正内联体都很小）\n                slots += used;",
  "                out.extend_from_slice(&bytes);\n                // 嵌套调用各占一段槽（保守：不共用，反正内联体都很小）\n                slots += used;\n                stack_used = stack_used.max(deep);"
);
s = s.replace("    Some((out, slots))\n}", "    Some((out, slots, stack_used))\n}");

// Callee 带上 max_stack
s = s.replace("    max_locals: usize,\n    instance: bool,", "    max_locals: usize,\n    max_stack: usize,\n    instance: bool,");
s = s.replace(
  "    let (max_locals, _max_stack, body, table) = code_of(&cf, m).ok()?;",
  "    let (max_locals, max_stack, body, table) = code_of(&cf, m).ok()?;"
);
s = s.replace(
  "    Some(Callee { cf, kinds, max_locals, instance: !is_static, body })",
  "    Some(Callee { cf, kinds, max_locals, max_stack, instance: !is_static, body })"
);

fs.writeFileSync(p, s);
console.log("inline.rs 改完");
