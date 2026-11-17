// 把 cases.mjs 按 (section, env) 分组写成 probe 需要的 manifest，并打印 run.sh 要跑的批次。
import fs from 'node:fs';
import path from 'node:path';
import { CASES } from './cases.mjs';

const ROOT = 'D:/vmless/bench';
const RAW = path.join(ROOT, 'raw');
fs.mkdirSync(RAW, { recursive: true });

const groups = new Map();
for (const cs of CASES) {
  const envKey = Object.keys(cs.env).sort().join(',');
  const gk = `${cs.section}__${envKey || 'noenv'}__r${cs.reps}`;
  if (!groups.has(gk)) groups.set(gk, { section: cs.section, env: cs.env, cases: [] });
  groups.get(gk).cases.push(cs);
}

const batches = [];
for (const [gk, g] of groups) {
  const manifest = {
    // probe 的 manifest 是扁平 JSON 对象数组；reps 取组内最大值，个别用例更少的重复次数由
    // summarize 阶段按实际行数处理（多余的重复行对 min/median 没坏处）
    cases: g.cases.map((c) => ({
      id: c.id, exe: c.exe, args: c.args, timeoutMs: c.timeoutMs,
    })),
    reps: Math.max(...g.cases.map((c) => c.reps)),
  };
  // probe 自己的格式：[{id,exe,args,timeoutMs}] —— reps 走命令行
  const file = path.join(RAW, `manifest-${gk}.json`);
  fs.writeFileSync(file, JSON.stringify(manifest.cases, null, 1));
  const out = path.join(RAW, `${gk}.jsonl`);
  batches.push({ file, out, env: g.env, reps: manifest.reps, gk });
}

fs.writeFileSync(path.join(RAW, 'batches.json'), JSON.stringify(batches, null, 1));
for (const b of batches) {
  const envArgs = Object.entries(b.env).map(([k, v]) => `--env ${k}=${v}`).join(' ');
  console.log([b.reps, b.file, b.out, envArgs].join('\t'));
}
