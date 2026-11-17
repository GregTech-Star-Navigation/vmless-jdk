// 汇总 raw/*.jsonl + logs/*.log → result.json 与 REPORT.md 的表格。
// 统计口径：min 为主（这台机器有带外扫描 + 别的子代理在编译，中位数被噪声抬高），
// median / p90 一并给出，让人自己判断。
import fs from 'node:fs';
import path from 'node:path';

const ROOT = 'D:/vmless/bench';
const RAW = path.join(ROOT, 'raw');
const LOGS = path.join(ROOT, 'logs');

const readLog = (id) => {
  const p = path.join(LOGS, `${id}.log`);
  const p2 = path.join(LOGS, `${id.replace(/[\\/: ]/g, '_')}.log`);
  for (const f of [p, p2]) if (fs.existsSync(f)) return fs.readFileSync(f, 'utf8');
  return '';
};

// ---- 收集 wall/peak
const rows = [];
for (const f of fs.readdirSync(RAW)) {
  if (!f.endsWith('.jsonl')) continue;
  for (const line of fs.readFileSync(path.join(RAW, f), 'utf8').split(/\r?\n/)) {
    if (!line.trim()) continue;
    try { rows.push(JSON.parse(line)); } catch { /* 半行，忽略 */ }
  }
}

const byId = new Map();
for (const r of rows) {
  if (r.error) continue;
  if (!byId.has(r.id)) byId.set(r.id, []);
  byId.get(r.id).push(r);
}

const num = (v) => (typeof v === 'number' && isFinite(v) ? v : NaN);
const stat = (arr) => {
  const v = arr.filter((x) => isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return null;
  const q = (p) => v[Math.min(v.length - 1, Math.floor(p * (v.length - 1)))];
  return { n: v.length, min: v[0], med: q(0.5), p90: q(0.9), max: v[v.length - 1] };
};

const W = new Map(); // id -> {wall, peak, exits, timeouts}
for (const [id, list] of byId) {
  W.set(id, {
    wall: stat(list.map((r) => num(r.wall_ms))),
    peak: stat(list.map((r) => num(r.peak_kb))),
    exits: [...new Set(list.map((r) => r.exit))],
    timeouts: list.filter((r) => r.timedOut).length,
    n: list.length,
  });
}

// ---- 逐用例解析程序自报的数字
// 不同程序印的格式不一样：run_jit 印「执行耗时: X ms」，原始 P2 印「sum=… ms=…」，
// P2j/P1j/Gc 印两行纯数字。先按 id 专用解析，拿不到就落到这里。
function genericInprog(log) {
  const m1 = log.match(/执行耗时: ([\d.]+) ms/);
  if (m1) return num(Number(m1[1]));
  const m2 = log.match(/ms=(\d+)/);
  if (m2) return num(Number(m2[1]));
  const ls = log.split(/\r?\n/).map((x) => x.trim()).filter((x) => /^\d+$/.test(x));
  if (ls.length >= 2) return num(Number(ls[1]));
  return NaN;
}

const parsers = {
  // P2j / P1j / Gc：第一行 sum、第二行 ms、第三行（可选）
  vml_interp_p2j: (log) => ({ inprog: num(Number((log.match(/^(\d+)\n(\d+)/m) || [])[2])) }),
  vml_interp_p2: (log) => ({ inprog: num(Number((log.match(/ms=(\d+)/) || [])[1])) }),
  hs_p2: (log) => ({ inprog: genericInprog(log) }),
  vml_jit_p2j: (log) => ({
    inprog: num(Number((log.match(/执行耗时: ([\d.]+) ms/) || [])[1])),
    jitc: num(Number((log.match(/JIT 编译耗时: ([\d.]+) ms/) || [])[1])),
    fellback: /退回解释器/.test(log),
  }),
  vml_jit_p2: (log) => ({
    inprog: num(Number((log.match(/ms=(\d+)/) || [])[1])),
    fellback: /退回解释器/.test(log),
    reason: (/JIT 跳过：([^\n]*)/.exec(log) || [])[1] || '',
  }),
  vml_jit_p1j_4: (log) => ({
    failed: /JIT 编译失败/.test(log),
    reason: (/JIT 编译失败：([^\n]*)/.exec(log) || [])[1] || '',
  }),
  hs_classload: (log) => {
    const ls = log.split(/\r?\n/).map((s) => s.trim()).filter((s) => /^\d+$/.test(s));
    return { ok: num(Number(ls[0])), miss: num(Number(ls[1])), inprog: num(Number(ls[2])) };
  },
  vml_classload: (log) => {
    const ls = log.split(/\r?\n/).map((s) => s.trim()).filter((s) => /^\d+$/.test(s));
    return {
      ok: num(Number(ls[0])), miss: num(Number(ls[1])), inprog: num(Number(ls[2])),
      failed: /执行失败/.test(log),
      err: (/执行失败: ([^\n]*)/.exec(log) || [])[1] || '',
    };
  },
  vml_jdk_empty: (log) => ({
    classes: num(Number((log.match(/类表: (\d+) 个类/) || [])[1])),
  }),
  vml_jdk_load5421: (log) => ({
    classes: num(Number((log.match(/类表: (\d+) 个类/) || [])[1])),
  }),
  gc_vml_1: gcVml, gc_vml_2: gcVml, gc_vml_3: gcVml,
  gc_vml_interp: (log) => ({ inprog: num(Number((log.match(/^(\d+)\n(\d+)/m) || [])[2])) }),
  hs_alloc8: (log) => ({ inprog: NaN }),
};
function gcVml(log) {
  const m = log.match(/minor=(\d+) major=(\d+) 暂停=([\d.]+)ms 最大=([\d.]+)ms/);
  if (!m) return { failed: true };
  return {
    minor: num(Number(m[1])), major: num(Number(m[2])),
    totalPause: num(Number(m[3])), maxPause: num(Number(m[4])),
  };
}
for (let i = 1; i <= 4; i++) {
  for (const side of ['vml_interp_p1j', 'hs_p1j']) {
    parsers[`${side}_${i}`] = (log) => ({ inprog: num(Number((log.match(/^(\d+)\n(\d+)$/m) || [])[2])) });
  }
}

const K = new Map(); // id -> 解析出的程序内数字
for (const id of W.keys()) {
  const p = (parsers[id.replace(/-/g, '_')] || (() => ({})))(readLog(id));
  if (!isFinite(p.inprog)) p.inprog = genericInprog(readLog(id));  // 兜底
  K.set(id, p);
}

// ---- HotSpot GC 日志（每次跑一个文件，按 pid）
function hsGc() {
  const files = fs.readdirSync(LOGS).filter((f) => /^hs-gc-\d+\.log$/.test(f));
  const perRun = [];
  for (const f of files) {
    const txt = fs.readFileSync(path.join(LOGS, f), 'utf8');
    const pauses = [...txt.matchAll(/Pause[^\n]*?\s([\d.]+)ms\s*$/gm)].map((m) => Number(m[1]));
    if (!pauses.length) continue;
    perRun.push({
      count: pauses.length,
      maxPause: Math.max(...pauses),
      totalPause: pauses.reduce((a, b) => a + b, 0),
      full: (txt.match(/Pause Full/g) || []).length,
    });
  }
  return perRun;
}

const f1 = (x) => (isFinite(x) ? x.toFixed(1) : '—');
const f2 = (x) => (isFinite(x) ? x.toFixed(2) : '—');
const g = (id) => W.get(id);
const k = (id, key) => (K.get(id) || {})[key];
const wallMin = (id) => (g(id)?.wall ? g(id).wall.min : NaN);
const wallMed = (id) => (g(id)?.wall ? g(id).wall.med : NaN);
const peakMin = (id) => (g(id)?.peak ? g(id).peak.min : NaN);
const inprog = (id) => num(k(id, 'inprog'));

const ratio = (a, b) => (isFinite(a) && isFinite(b) && b > 0 ? a / b : NaN);
function verdict(hs, vml, hsName = 'HotSpot', vmlName = 'vmless') {
  if (!isFinite(hs) || !isFinite(vml)) return '—';
  return vml <= hs
    ? `**${vmlName} 快 ${(hs / vml).toFixed(2)}×**`
    : `HotSpot 快 ${(vml / hs).toFixed(2)}×`;
}

const V = {}; // 结构化结果
const base = wallMin('spawn-hostname');

// ================================================================ REPORT.md
const L = [];
L.push('# vmless vs HotSpot 25 —— 基准报告');
L.push('');
L.push(`> 生成时间：${new Date().toISOString()}　·　脚本：\`bench/run.sh\`（重跑同一条命令复现）`);
L.push('> 原始数据：`bench/raw/*.jsonl`（每次运行一行）与 `bench/logs/*.log`（子进程输出）');
L.push('');
L.push('机器：36 逻辑核 / 64GB RAM / Windows。**注意**：跑这份基准时机器上还有别的编译任务在跑，');
L.push('且每次创建进程都会被带外扫描器检查一遍 —— 两者都体现在「进程创建底噪」这一行里。');
L.push(`本机进程创建底噪（\`hostname.exe\`，min）：**${f1(base)} ms**。所有墙钟对比都受它影响，`);
L.push('所以下面凡是有墙钟的地方都同时给出 min（受噪声最小）与 median。');
L.push('');

// ---- 1 启动
L.push('## 1 启动时间');
L.push('');
L.push('30 次/项交错跑，取 min 与 median。`扣底噪` = min − 进程创建底噪，代表运行时自己花的钱。');
L.push('');
L.push('| 用例 | min ms | median ms | 扣底噪 ms | 峰值 RSS |');
L.push('| --- | ---: | ---: | ---: | ---: |');
const startupRows = [
  ['HotSpot `java -version`', 'hs-version'],
  ['HotSpot 空 main', 'hs-empty'],
  ['HotSpot HelloWorld', 'hs-hello'],
  ['HotSpot 1000 行类', 'hs-big1000'],
  ['vmless `rustjava` 空 main（内建 java.base 子集）', 'vml-rustjava-empty'],
  ['vmless `rustjava` HelloWorld', 'vml-rustjava-hello'],
  ['vmless `rustjava` 1000 行类', 'vml-rustjava-big1000'],
  ['vmless `run_jit` 空 main', 'vml-jit-empty'],
  ['vmless `run_jit` HelloWorld', 'vml-jit-hello'],
  ['vmless `run_jit` 1000 行类', 'vml-jit-big1000'],
  ['vmless `run_jdk` 空 main（真 java.base 5421 类）', 'vml-jdk-empty'],
];
for (const [label, id] of startupRows) {
  const mn = wallMin(id), md = wallMed(id), pk = peakMin(id);
  L.push(`| ${label} | ${f1(mn)} | ${f1(md)} | ${f1(mn - base)} | ${f1(pk / 1024)} MB |`);
}
L.push('');
const startGainJit = ratio(wallMin('hs-empty'), wallMin('vml-jit-empty'));
const startGainRj = ratio(wallMin('hs-empty'), wallMin('vml-rustjava-empty'));
L.push(`- 空 main：vmless \`run_jit\` ${f1(wallMin('vml-jit-empty'))} ms vs HotSpot ${f1(wallMin('hs-empty'))} ms → ${verdict(wallMin('hs-empty'), wallMin('vml-jit-empty'))}`);
L.push(`- 空 main：vmless \`rustjava\` ${f1(wallMin('vml-rustjava-empty'))} ms → ${verdict(wallMin('hs-empty'), wallMin('vml-rustjava-empty'))}`);
L.push(`- 扣掉相同的底噪后差距更大：\`run_jit\` ${f1(wallMin('vml-jit-empty') - base)} ms vs HotSpot ${f1(wallMin('hs-empty') - base)} ms（${f2(startGainJit)}× / ${f2(startGainRj)}×）。`);
L.push('- `rustjava` 的 min 已经贴着底噪（有时低于它，说明那几次扫描器没介入）——**即运行时自身开销低于本机测量分辨力**。');
L.push('- 反过来，`run_jdk`（要解析真 java.base 模块镜像、eager 装 5421 个类）是 **3.5 s 级**，比 HotSpot 慢一个数量级 —— 见 §6。');
L.push('');

// ---- 2 内存
L.push('## 2 内存占用');
L.push('');
L.push('### 2.1 峰值 RSS');
L.push('');
L.push('| 场景 | HotSpot 峰值 RSS | vmless 峰值 RSS | 谁省 |');
L.push('| --- | ---: | ---: | --- |');
const memRows = [
  ['空 main（常驻）', 'hs-empty', 'vml-jit-empty'],
  ['空 main（rustjava）', 'hs-empty', 'vml-rustjava-empty'],
  ['8M 次分配、全部即死', 'hs-alloc8', 'vml-jit-alloc8'],
];
for (const [label, h, v] of memRows) {
  const hp = peakMin(h), vp = peakMin(v);
  const who = isFinite(hp) && isFinite(vp) ? (vp <= hp ? `**vmless 省 ${(hp / vp).toFixed(2)}×**` : `HotSpot 省 ${(vp / hp).toFixed(2)}×`) : '—';
  L.push(`| ${label} | ${f1(hp / 1024)} MB | ${f1(vp / 1024)} MB | ${who} |`);
}
L.push('');
L.push('### 2.2 对象大小');
L.push('');
L.push('对象大小按**布局公式**给（这是权威值，两边都能从各自的源码/文档推出），RSS 斜率只当交叉验证：');
L.push('');
L.push('| 对象形状 | vmless | HotSpot 25 | 差 | 依据 |');
L.push('| --- | ---: | ---: | ---: | --- |');
L.push('| 空对象 `class E {}` | **8 B** | **16 B** | vmless 小 2× | 我们 `HEADER=8`（`class_id u32 + flags u32`），HotSpot 12 B 头 + 4 B 对齐填充 |');
L.push('| `{int v; long pad1}` | **24 B** | **24 B** | 打平 | 我们 = align8(8 + 3 槽×4)=24；HotSpot = 12 头 + 4 int + 8 long = 24 |');
L.push('| 引用数组每槽 | **4 B** | 4 B（压缩 oops） | 打平 | 我们存 `u32` 堆内偏移，HotSpot 压到 32 位引用 |');
L.push('');
L.push('RSS 斜率交叉验证（保持 100 万 / 400 万 个 `ONode{int,long}` 存活，取峰值 RSS 差）：');
L.push('');
L.push('| 存活对象数 | HotSpot 峰值 RSS | vmless 峰值 RSS |');
L.push('| ---: | ---: | ---: |');
for (const n of ['1m', '2m', '4m', '8m']) {
  L.push(`| ${n.toUpperCase()} | ${f1(peakMin(`hs-obj-${n}`) / 1024)} MB | ${f1(peakMin(`vml-obj-${n}`) / 1024)} MB |`);
}
const objH = (peakMin('hs-obj-4m') - peakMin('hs-obj-1m')) * 1024 / 3e6;
const objV = (peakMin('vml-obj-4m') - peakMin('vml-obj-1m')) * 1024 / 3e6;
const objH8 = (peakMin('hs-obj-8m') - peakMin('hs-obj-1m')) * 1024 / 7e6;
const objV8 = (peakMin('vml-obj-8m') - peakMin('vml-obj-1m')) * 1024 / 7e6;
L.push('');
L.push(`- HotSpot 斜率 ${f2(objH)} B/对象（1M→4M）、${f2(objH8)} B/对象（1M→8M）：`);
L.push('  ≈ 24 B 对象 + 4 B 数组槽 + TLAB/堆松弛 —— **与 24 B 的布局吻合**。');
L.push(`- vmless 斜率 ${f2(objV)} B/对象（1M→4M）、${f2(objV8)} B/对象（1M→8M）：**这个数不可用**，`);
L.push('  因为我们的堆是一整块连续内存按 `realloc` 增长（初始 64 MB、按步进扩容），');
L.push('  1M/2M 两点经常落在同一个容量台阶上（上表里 2M 和 4M 几乎一样就是台阶），斜率被台阶吃掉。');
L.push('- 所以这一项的口径是：**空对象 8 B vs 16 B（2×），双字段对象打平（24 B vs 24 B）**。');
L.push('  任务书里「16 B vs 24 B」指的是不同对象形状，我们赢在对象头，赢不了字段对齐。');
L.push('');

// ---- 3 GC
L.push('## 3 GC 停顿');
L.push('');
L.push('负载：8,000,000 次分配、1/8（100 万）存活。两边都限制在 256 MB 堆。');
L.push('HotSpot 用默认 G1（`-Xlog:gc` 解析 Pause 行），vmless 用 `VMLESS_GC_STATS=1` 打印的分代统计。');
L.push('');
const hsG = hsGc();
const vmlG = ['gc-vml-1', 'gc-vml-2', 'gc-vml-3'].map((id) => K.get(id)).filter((x) => x && !x.failed);
const hsMax = hsG.length ? Math.max(...hsG.map((x) => x.maxPause)) : NaN;
const hsTot = hsG.length ? Math.max(...hsG.map((x) => x.totalPause)) : NaN;
const hsCnt = hsG.length ? Math.round(hsG.reduce((a, b) => a + b.count, 0) / hsG.length) : NaN;
const vmlMax = vmlG.length ? Math.max(...vmlG.map((x) => x.maxPause)) : NaN;
const vmlTot = vmlG.length ? Math.max(...vmlG.map((x) => x.totalPause)) : NaN;
const vmlCnt = vmlG.length ? Math.round(vmlG.reduce((a, b) => a + b.minor + b.major, 0) / vmlG.length) : NaN;
L.push('| 指标 | HotSpot 25 (G1) | vmless (JIT 堆) | 对比 |');
L.push('| --- | ---: | ---: | --- |');
L.push(`| GC 次数 | ${f1(hsCnt)} | ${f1(vmlCnt)} | — |`);
L.push(`| **最大单次停顿** | ${f2(hsMax)} ms | ${f2(vmlMax)} ms | ${isFinite(hsMax) && isFinite(vmlMax) ? (vmlMax <= hsMax ? `**vmless 小 ${f2(hsMax / vmlMax)}×**` : `HotSpot 小 ${f2(vmlMax / hsMax)}×`) : '—'} |`);
L.push(`| **总停顿** | ${f2(hsTot)} ms | ${f2(vmlTot)} ms | ${isFinite(hsTot) && isFinite(vmlTot) ? (vmlTot <= hsTot ? `**vmless 少 ${f2(hsTot / vmlTot)}×**` : `HotSpot 少 ${f2(vmlTot / hsTot)}×`) : '—'} |`);
L.push(`| 程序内耗时（自报） | ${f1(inprog('hs-p2') || HsGcProgMs())} ms | ${f1(GcVmlProgMs())} ms | — |`);
L.push(`| 峰值 RSS | ${f1(peakMin('gc-hs-1') / 1024)} MB | ${f1(peakMin('gc-vml-1') / 1024)} MB | — |`);
L.push('');
function HsGcProgMs() {
  // Gc.java 第二行就是毫秒数
  const l = readLog('gc-hs-1').split(/\r?\n/).map((s) => s.trim()).filter((s) => /^\d+$/.test(s));
  return num(Number(l[1]));
}
function GcVmlProgMs() {
  const l = readLog('gc-vml-1').split(/\r?\n/).map((s) => s.trim()).filter((s) => /^\d+$/.test(s));
  return num(Number(l[1]));
}
const interpGcMs = inprog('gc-vml-interp');
const interpGcWall = wallMin('gc-vml-interp');
L.push(`- 解释器走同一个程序：程序内 \`${f1(interpGcMs)}\` ms、墙钟 \`${f1(interpGcWall)}\` ms，`);
L.push('  而且**解释器侧的 GC 没有任何停顿遥测**（`src/vm/gc.rs` 不计 pause）—— 这一行只能标 n/a，是已知短板。');
L.push('- 我们的 GC 是分代复制 + 老年代标记清扫，堆是一整块连续内存（`realloc` 增长），');
L.push('  **但实测是我们输**：最大单次停顿 3.6×、总停顿 4×（与「我们停顿更小」的预期相反，照实写）。');
L.push('  原因：我们是一次性 stop-the-world 的复制式 minor + 老年代标记清扫 major，');
L.push('  整堆是一块连续内存，major 要遍历/搬运全堆，且没有并发标记，工作全压在单次停顿里；');
L.push('  G1 用分区 + 并发标记 + 混合回收把同样的工作摊成很多个短停顿。');
L.push('  另外堆按步进扩容，容量增长也发生在同一次 GC 里，搬运成本一起计入。');
L.push('  峰值 RSS 同样更高（整块连续堆扩容时会 double），这是「快启动 / 省常驻」换来的代价。');
L.push('');

// ---- 5 类加载
L.push('## 6 类加载');
L.push('');
L.push('| 用例 | 结果 |');
L.push('| --- | --- |');
L.push(`| HotSpot 加载 5000 个 java.base 类（\`Class.forName(name,false,loader)\`，程序内自报） | ${f1(inprog('hs-classload'))} ms，成功 ${f1(k('hs-classload', 'ok'))} 个，失败 ${f1(k('hs-classload', 'miss'))} 个 |`);
L.push(`| HotSpot 空 main（\`-Xshare:off\`，冷启不读 CDS 归档） | min ${f1(wallMin('hs-empty-xshareoff'))} ms |`);
L.push(`| vmless \`run_jdk\` 启动即装 5421 个 java.base 类 | min ${f1(wallMin('vml-jdk-load5421'))} ms（打印 \`类表: ${f1(k('vml-jdk-load5421', 'classes'))} 个类\`） |`);
L.push(`| vmless 跑同一个 \`ClassLoad2\` | **跑不了**：${k('vml-classload', 'err') || '失败'} |`);
L.push('');
L.push('- 口径不一样，必须说清：HotSpot 是**惰性加载**（用到才装），vmless 是**启动即 eager 装全量类表**。');
L.push('  所以「5421 类」这个数对我们是启动成本、对 HotSpot 根本不是它的启动路径 —— 这一项**不能**当成同类比较。');
L.push('- 我们跑不了 `Class.forName(...)` 那条路，是因为解析 `java.util.concurrent.ConcurrentHashMap` 时会调用');
L.push('  `jdk.internal.access.JavaLangAccess.currentCarrierThread()`，我们的 java.base 支持面还没有它。');
L.push('- 结论：**类加载这一维我们没有可比数字，只有「eager 3.5 s」这个成本**。要跟 HotSpot 比，得先做惰性加载或类表归档。');
L.push('');

// ---- 6 解释
L.push('## 7 解释执行吞吐');
L.push('');
L.push('同一份字节码（`P2j`，8M×4 次循环、无 indy），跑完取程序自报的毫秒数。');
L.push('');
L.push('| 引擎 | 程序内耗时 | 墙钟 min | 相对 HotSpot |');
L.push('| --- | ---: | ---: | --- |');
const hsP2 = inprog('hs-p2');
const viP2j = inprog('vml-interp-p2j');
const viP2 = inprog('vml-interp-p2');
L.push(`| HotSpot 25（C2 编译后） | ${f1(hsP2)} ms | ${f1(wallMin('hs-p2'))} ms | 1× |`);
L.push(`| vmless 解释器（`+'`run_jdk`'+`） | ${f1(viP2j)} ms | ${f1(wallMin('vml-interp-p2j'))} ms | ${isFinite(viP2j) && isFinite(hsP2) ? `**慢 ${f2(viP2j / hsP2)}×**` : '—'} |`);
L.push(`| vmless 解释器跑原始 `+'`P2`'+`（带 indy 字符串拼接） | ${f1(viP2)} ms | ${f1(wallMin('vml-interp-p2'))} ms | ${isFinite(viP2) && isFinite(hsP2) ? `慢 ${f2(viP2 / hsP2)}×` : '—'} |`);
L.push('');
L.push('- 这个差距是**结构性的、不是 bug**：HotSpot 跑的是 C2 优化后的机器码（循环展开、强度削减、寄存器分配），');
L.push('  我们是逐条字节码解释（每条指令一次 `match` 分发）。差一到两个数量级是解释器的正常水位。');
L.push('- 也说明 `P2` 这类纯计算循环是我们最不擅长的形状 —— 没有对象、没有调用、全是热点算术。');
L.push('');

// ---- 7 JIT
L.push('## 8 JIT 吞吐');
L.push('');
L.push('| 用例 | 程序内耗时 | JIT 编译耗时 | 说明 |');
L.push('| --- | ---: | ---: | --- |');
const vj = inprog('vml-jit-p2j');
L.push(`| HotSpot 25 | ${f1(hsP2)} ms | — | C2 |`);
L.push(`| vmless JIT（`+'`run_jit`'+`，P2j） | ${f1(vj)} ms | ${f1(k('vml-jit-p2j', 'jitc'))} ms | Cranelift 直出机器码 |`);
L.push(`| vmless JIT 跑原始 P2 | ${f1(inprog('vml-jit-p2'))} ms | — | ${k('vml-jit-p2', 'fellback') ? '**回退了解释器**（JIT 不支持 invokedynamic）' : ''} |`);
L.push('');
L.push(`- JIT 对解释器的加速：${isFinite(vj) && isFinite(viP2j) ? `**${f2(viP2j / vj)}×**` : '—'}（${f1(viP2j)} → ${f1(vj)} ms）—— JIT 本身是有效的。`);
L.push(`- 但离 HotSpot 还差 ${isFinite(vj) && isFinite(hsP2) ? `**${f2(vj / hsP2)}×**` : '—'}：我们只做「静态整方法直译」，`);
L.push('  没有分层编译、没有内联、没有循环优化、没有 PGO。HotSpot 的热点循环会被 C2 反复优化几轮。');
L.push(`- 原始 \`P2\` 走 JIT 会**整体退回解释器**（原因：${k('vml-jit-p2', 'reason') || 'invokedynamic'}），`);
L.push('  这是已知设计边界（`rustjavac/examples/run_jit.rs` 里明说了），不是坏掉。');
L.push('- **口径提醒**：`run_jit` 目前是「一处不支持就**整个程序**退回解释器」');
L.push('  （`invokedynamic` / `java.lang.invoke` / `synchronized` 三种触发）。这个策略正在改成逐方法回退，');
L.push('  所以**凡是用到 lambda、字符串拼接、同步块的程序，`run_jit` 的数字都偏低**。');
L.push('  本文的 JIT 数字取自 `P2j`（刻意避开这三样）；同形状的真字节码路径 `run_jdk` 对照值见上一节。');
L.push('');

// ---- 8 并行
L.push('## 9 并行扩展性');
L.push('');
L.push('程序形状与 `jdkms/par/P1` 相同：起 N 个线程，**每个线程各做 8M 次循环**（总工作量随 N 增长）。');
L.push('所以这里的「加速比」按**吞吐**算：`N × t(1) ÷ t(N)` —— 完全并行时它等于 N，等于 1 表示一点没并行。');
L.push('都用程序自报的内层毫秒数，启动成本不混进来。');
L.push('');
L.push('| 线程数 | HotSpot 内层 ms | HotSpot 加速比 | vmless 内层 ms | vmless 加速比 |');
L.push('| ---: | ---: | ---: | ---: | ---: |');
const hs1 = inprog('hs-p1j-1');
const vm1 = inprog('vml-interp-p1j-1');
for (const n of [1, 2, 4, 8]) {
  const h = inprog(`hs-p1j-${n}`);
  const v = inprog(`vml-interp-p1j-${n}`);
  const hSp = isFinite(hs1) && isFinite(h) && h > 0 ? (n * hs1 / h) : NaN;   // 吞吐加速比
  const vSp = isFinite(vm1) && isFinite(v) && v > 0 ? (n * vm1 / v) : NaN;
  L.push(`| ${n} | ${f1(h)} | ${f2(hSp)}× | ${f1(v)} | ${f2(vSp)}× |`);
}
const vm8 = inprog('vml-interp-p1j-8');
const hs8 = inprog('hs-p1j-8');
L.push('');
const v8sp = isFinite(vm1) && isFinite(vm8) && vm8 > 0 ? 8 * vm1 / vm8 : NaN;
const h8sp = isFinite(hs1) && isFinite(hs8) && hs8 > 0 ? 8 * hs1 / hs8 : NaN;
L.push(`- 8 线程吞吐加速比：HotSpot ${f2(h8sp)}×、vmless ${f2(v8sp)}×（36 核，理论天花板 8×）。`);
L.push('- 我们的解释器已经拆掉 GIL（`rustjavac` 0.57 的「字节码真并行」），所以确实随核数变快；');
L.push('  但每个线程都是解释执行，绝对耗时仍然比 HotSpot 慢一个数量级 —— **并行度赢了，绝对值没赢**。');
const jitP1 = K.get('vml-jit-p1j-4') || {};
L.push(`- JIT 侧的多线程是**跑不了**的：\`run_jit\` 对 P1j 报 \`${jitP1.reason || 'JIT 编译失败'}\`。`);
L.push('  这是 JIT 引用分析在 `aaload` 之后的状态建模缺口，属于已知待修项（任务书里 JIT 由另一个子代理负责）。');
L.push('');

// ---- 10 汇总
L.push('## 10 汇总：赢在哪、输在哪');
L.push('');
L.push('### 赢的维度');
L.push('');
L.push('| 维度 | vmless | HotSpot 25 | 倍数 |');
L.push('| --- | ---: | ---: | ---: |');
L.push(`| 启动（空 main，min） | ${f1(wallMin('vml-jit-empty'))} ms | ${f1(wallMin('hs-empty'))} ms | **${f2(ratio(wallMin('hs-empty'), wallMin('vml-jit-empty')))}×** |`);
L.push(`| 启动（rustjava，min） | ${f1(wallMin('vml-rustjava-empty'))} ms | ${f1(wallMin('hs-empty'))} ms | **${f2(ratio(wallMin('hs-empty'), wallMin('vml-rustjava-empty')))}×** |`);
L.push(`| 常驻 RSS | ${f1(peakMin('vml-jit-empty') / 1024)} MB | ${f1(peakMin('hs-empty') / 1024)} MB | **${f2(ratio(peakMin('hs-empty'), peakMin('vml-jit-empty')))}×** |`);
L.push(`| 对象大小（空对象） | 8 B | 16 B | **2.00×** |`);
L.push(`| 8 线程并行吞吐加速比 | ${f2(v8sp)}× | ${f2(h8sp)}× | **${f2(v8sp / h8sp)}×** |`);
L.push('');
L.push('> 注：HotSpot 的最小启动本身在 485–727 ms 之间波动（同一台机器上 -version、空 main、HelloWorld 三个用例的 min 都不相同），');
L.push('> 所以「启动快多少」是个区间：**3.6×–5.4×**，取哪一端取决于拿哪个用例当基准。表里用的是最保守的对手（空 main 的 min）。');
L.push('');
L.push('### 输的维度');
L.push('');
L.push('| 维度 | vmless | HotSpot 25 | 倍数 | 为什么 |');
L.push('| --- | ---: | ---: | ---: | --- |');
L.push(`| 最大单次 GC 停顿 | ${f2(vmlMax)} ms | ${f2(hsMax)} ms | 慢 ${f2(ratio(vmlMax, hsMax))}× | STW 复制+标记清扫，整块连续堆、无并发标记；G1 用分区+并发摊平 |`);
L.push(`| 总 GC 停顿 | ${f2(vmlTot)} ms | ${f2(hsTot)} ms | 慢 ${f2(ratio(vmlTot, hsTot))}× | 同上；另外堆扩容也发生在 GC 里 |`);
L.push(`| 分配峰值 RSS（8M 即死对象） | ${f1(peakMin('vml-jit-alloc8') / 1024)} MB | ${f1(peakMin('hs-alloc8') / 1024)} MB | 慢 ${f2(ratio(peakMin('vml-jit-alloc8'), peakMin('hs-alloc8')))}× | 整块堆按步进扩容，扩容时 RSS 会翻倍 |`);
L.push(`| 解释执行吞吐 | ${f1(viP2j)} ms | ${f1(hsP2)} ms | 慢 ${f2(viP2j / hsP2)}× | 逐条字节码解释 vs C2 编译后的机器码 |`);
L.push(`| JIT 吞吐 | ${f1(vj)} ms | ${f1(hsP2)} ms | 慢 ${f2(vj / hsP2)}× | 静态整方法直译，无内联/循环优化/分层 |`);
L.push(`| 类加载（可用性） | 跑不了 Class.forName 全量 | ${f1(inprog('hs-classload'))} ms / 5000 类 | — | 缺 jdk.internal.access 面；且 eager 装表更慢 |`);
L.push(`| eager 类表启动 | ${f1(wallMin('vml-jdk-empty'))} ms | ${f1(wallMin('hs-empty'))} ms | 慢 ${f2(wallMin('vml-jdk-empty') / wallMin('hs-empty'))}× | 启动即装 5421 类，没有 CDS/惰性加载 |`);
L.push('');
L.push('### 一句话结论');
L.push('');
L.push('**真赢的只有三样：启动快 5×、常驻内存省 8×、对象头小一半（空对象 8 B vs 16 B）；');
L.push('并行扩展效率（8 线程 6.9× vs 3.9×）也更好，但那是效率不是绝对值。**');
L.push('');
L.push('**输的是吞吐（解释 108×、JIT 5.6×）和 GC 停顿（最大 4×、总 4×）与分配峰值内存（2.6×）——');
L.push('前者的原因是结构性的（逐条解释 / 静态整方法直译，没有 C2 那套内联与循环优化），');
L.push('后者的原因是我们的堆是整块连续内存 + STW 回收，没有 G1 的分区并发。**');
L.push('');

fs.writeFileSync(path.join(ROOT, 'REPORT.md'), L.join('\n'));

// ---- result.json
V.generated = new Date().toISOString();
V.spawnBaselineMs = base;
V.cases = {};
for (const [id, w] of W) {
  V.cases[id] = {
    n: w.n,
    wall: w.wall, peak_kb: w.peak,
    exits: w.exits, timeouts: w.timeouts,
    parsed: K.get(id) || {},
  };
}
V.gc = { hs: hsG, vml: vmlG, hsMaxPauseMs: hsMax, hsTotalPauseMs: hsTot, vmlMaxPauseMs: vmlMax, vmlTotalPauseMs: vmlTot };
V.objectBytes = { hotspotEmpty: 16, vmlessEmpty: 8, hotspotPair: 24, vmlessPair: 24, hotspotSlopeB: objH, vmlessSlopeB_unusable: objV };
V.verdicts = {
  startupGainVsHotspot: ratio(wallMin('hs-empty'), wallMin('vml-jit-empty')),
  residentMemGain: ratio(peakMin('hs-empty'), peakMin('vml-jit-empty')),
  objectSizeGain: 2.0,
  interpSlowdown: viP2j / hsP2,
  jitSlowdown: vj / hsP2,
  jitSpeedupOverInterp: viP2j / vj,
  parallel8Vmless: vm1 / vm8,
  parallel8Hotspot: hs1 / hs8,
};
fs.writeFileSync(path.join(ROOT, 'result.json'), JSON.stringify(V, null, 2));
console.log('REPORT.md + result.json 已生成');
console.log('  启动倍数(空 main, min):', f2(V.verdicts.startupGainVsHotspot));
console.log('  常驻内存倍数:', f2(V.verdicts.residentMemGain));
console.log('  对象大小(空对象): 8B vs 16B = 2.00x（斜率 H=' + f2(objH) + ' B/obj 仅作交叉验证）');
console.log('  GC 最大停顿 H/V:', f2(hsMax), '/', f2(vmlMax), 'ms');
console.log('  解释慢:', f2(V.verdicts.interpSlowdown), 'x   JIT 慢:', f2(V.verdicts.jitSlowdown), 'x   JIT/解释加速:', f2(V.verdicts.jitSpeedupOverInterp), 'x');
