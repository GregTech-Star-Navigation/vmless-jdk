const fs = require("fs");
let p = "src/jit/mod.rs", s = fs.readFileSync(p, "utf8");
s = s.replace(
  "    n_compiled: usize,",
  "    n_compiled: usize,\n    /// 内联掉的调用点个数（`jit::inline` 的账）\n    n_inlined: usize,"
);
s = s.replace("            n_compiled: 0,", "            n_compiled: 0,\n            n_inlined: 0,");
s = s.replace(
  '        format!(\n            "JIT: 编译 {} 个方法，解释 {} 个{}",\n            self.n_compiled,\n            self.interpreted.len(),\n            detail\n        )',
  '        format!(\n            "JIT: 编译 {} 个方法，内联 {} 个调用点，解释 {} 个{}",\n            self.n_compiled,\n            self.n_inlined,\n            self.interpreted.len(),\n            detail\n        )'
);
fs.writeFileSync(p, s);
console.log("mod.rs ok");

p = "src/jit/translate.rs";
s = fs.readFileSync(p, "utf8");
const before = s;
s = s.replace(
  "            let (b, ml) =\n                super::inline::inline_all(&self.classes, &cf, class, name, desc, &body, max_locals);\n            (b, ml)",
  "            let (b, ml, n) =\n                super::inline::inline_all(&self.classes, &cf, class, name, desc, &body, max_locals);\n            self.n_inlined += n;\n            (b, ml)"
);
if (s === before) console.log("translate.rs MISS");
else console.log("translate.rs ok");
fs.writeFileSync(p, s);
