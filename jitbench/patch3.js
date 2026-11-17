const fs = require("fs");

// ---- translate.rs: plan() 用 insn_len_at（switch 变长要对齐）----
let p = "src/jit/translate.rs";
let s = fs.readFileSync(p, "utf8");
let a = `            _ => {}
            }
            pc += insn_len(op);
        }
        let var = (0..max_locals)`;
if (!s.includes("pc += insn_len(op);\n        }\n        let var")) {
  console.log("plan 尾部未找到，尝试替换扫描步长");
}
s = s.replace(
  "                _ => {}\n            }\n            pc += insn_len(op);\n        }\n        let var",
  "                _ => {}\n            }\n            // 用 insn_len_at：tableswitch/lookupswitch 是变长的，按 op 猜长度会\n            // 走进 switch 的数据里，把偏移字节当成操作码（最坏只是多标一个保守类型，\n            // 但没理由留着这个噪声源）\n            pc += insn_len_at(body, pc).max(1);\n        }\n        let var"
);

// 内联要同时把 max_stack 抬起来：根区大小是 max_locals + max_stack
s = s.replace(
  "            let (b, ml, n) =\n                super::inline::inline_all(&self.classes, &cf, class, name, desc, &body, max_locals);\n            self.n_inlined += n;\n            (b, ml)",
  "            let (b, ml, ms, n) =\n                super::inline::inline_all(&self.classes, &cf, class, name, desc, &body, max_locals);\n            self.n_inlined += n;\n            (b, ml, ms)"
);
s = s.replace(
  "        let (body, max_locals) = {",
  "        let (body, max_locals, stack_extra) = {"
);
fs.writeFileSync(p, s);
console.log("translate.rs 改完");
