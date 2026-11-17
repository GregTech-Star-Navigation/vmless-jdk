// 基准用例定义 —— run.sh 从这份清单生成 probe 的 manifest。
// 每个用例：id / section / exe / args / env / reps / timeoutMs
// 同一个 (section,env) 组合会合成一次 probe 调用，env 相同的用例交错跑。

export const HS = 'C:/Program Files/Eclipse Adoptium/jdk-25.0.4.101-hotspot/bin/java.exe';
export const RJ = 'D:/vmless/bparv/rustjava.exe';      // 解释器 + 内建 java.base 子集
export const RJIT = 'D:/vmless/bparv/run_jit.exe';     // JIT（不支持 invokedynamic → 自动回退解释器）
export const RJDK = 'D:/vmless/bparv/run_jdk.exe';     // 解释器 + 真 java.base（模块镜像，5421 类）
export const OUT = 'D:/vmless/bench/out';
export const JMS = 'D:/vmless/jdkms/par/out';          // 任务指定的 P1/P2 原始 class

// H = HotSpot 25, V = vmless 0.57.0
const c = (section, id, engine, exe, args, opt = {}) => ({
  section, id, engine, exe, args,
  env: opt.env || {},
  reps: opt.reps ?? 3,
  timeoutMs: opt.timeoutMs ?? 300000,
});

export const CASES = [
  // ---------------------------------------------------------------- 1 启动时间
  // 进程创建底噪：这台机器每次 spawn 都要过带外扫描，先量出来好扣。
  c('startup', 'spawn-hostname', 'H', 'C:/Windows/System32/hostname.exe', [], { reps: 30, timeoutMs: 60000 }),

  c('startup', 'hs-version', 'H', HS, ['-version'], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'hs-empty', 'H', HS, ['-cp', `${OUT}/Empty`, 'Empty'], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'hs-hello', 'H', HS, ['-cp', `${OUT}/Hello`, 'Hello'], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'hs-big1000', 'H', HS, ['-cp', `${OUT}/Big1000`, 'Big1000'], { reps: 30, timeoutMs: 60000 }),

  c('startup', 'vml-rustjava-empty', 'V', RJ, [`${OUT}/Empty/Empty.class`], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'vml-rustjava-hello', 'V', RJ, [`${OUT}/Hello/Hello.class`], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'vml-rustjava-big1000', 'V', RJ, [`${OUT}/Big1000/Big1000.class`], { reps: 30, timeoutMs: 60000 }),

  c('startup', 'vml-jit-empty', 'V', RJIT, [`${OUT}/Empty/Empty.class`], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'vml-jit-hello', 'V', RJIT, [`${OUT}/Hello/Hello.class`], { reps: 30, timeoutMs: 60000 }),
  c('startup', 'vml-jit-big1000', 'V', RJIT, [`${OUT}/Big1000/Big1000.class`], { reps: 30, timeoutMs: 60000 }),

  c('startup', 'vml-jdk-empty', 'V', RJDK, [`${OUT}/Empty`, 'Empty'], { reps: 5, timeoutMs: 120000 }),

  // ---------------------------------------------------------------- 2 内存占用
  c('memory', 'hs-alloc8', 'H', HS, ['-cp', `${OUT}/Alloc8`, 'Alloc8'], { reps: 7, timeoutMs: 120000 }),
  c('memory', 'vml-jit-alloc8', 'V', RJIT, [`${OUT}/Alloc8/Alloc8.class`], { reps: 7, timeoutMs: 300000 }),

  // 对象大小的斜率法：1M / 2M / 4M 全部存活
  c('memory', 'hs-obj-1m', 'H', HS, ['-cp', `${OUT}/ObjSize`, 'ObjSize1000000'], { reps: 5, timeoutMs: 120000 }),
  c('memory', 'hs-obj-2m', 'H', HS, ['-cp', `${OUT}/ObjSize`, 'ObjSize2000000'], { reps: 5, timeoutMs: 120000 }),
  c('memory', 'hs-obj-4m', 'H', HS, ['-cp', `${OUT}/ObjSize`, 'ObjSize4000000'], { reps: 5, timeoutMs: 180000 }),
  c('memory', 'vml-obj-1m', 'V', RJIT, [`${OUT}/ObjSize/ObjSize1000000.class`], { reps: 5, timeoutMs: 180000 }),
  c('memory', 'vml-obj-2m', 'V', RJIT, [`${OUT}/ObjSize/ObjSize2000000.class`], { reps: 5, timeoutMs: 240000 }),
  c('memory', 'vml-obj-4m', 'V', RJIT, [`${OUT}/ObjSize/ObjSize4000000.class`], { reps: 5, timeoutMs: 300000 }),


  // 空对象（只有对象头）：最干净的对象大小对照
  c('memory', 'hs-objE-1m', 'H', HS, ['-cp', `${OUT}/ObjSizeE`, 'ObjSizeE1000000'], { reps: 5, timeoutMs: 120000 }),
  c('memory', 'hs-objE-4m', 'H', HS, ['-cp', `${OUT}/ObjSizeE`, 'ObjSizeE4000000'], { reps: 5, timeoutMs: 180000 }),
  c('memory', 'hs-objE-8m', 'H', HS, ['-cp', `${OUT}/ObjSizeE`, 'ObjSizeE8000000'], { reps: 5, timeoutMs: 240000 }),
  c('memory', 'vml-objE-1m', 'V', RJIT, [`${OUT}/ObjSizeE/ObjSizeE1000000.class`], { reps: 5, timeoutMs: 180000 }),
  c('memory', 'vml-objE-4m', 'V', RJIT, [`${OUT}/ObjSizeE/ObjSizeE4000000.class`], { reps: 5, timeoutMs: 240000 }),
  c('memory', 'vml-objE-8m', 'V', RJIT, [`${OUT}/ObjSizeE/ObjSizeE8000000.class`], { reps: 5, timeoutMs: 300000 }),
  c('memory', 'hs-obj-8m', 'H', HS, ['-cp', `${OUT}/ObjSize`, 'ObjSize8000000'], { reps: 3, timeoutMs: 240000 }),
  c('memory', 'vml-obj-8m', 'V', RJIT, [`${OUT}/ObjSize/ObjSize8000000.class`], { reps: 3, timeoutMs: 300000 }),

  // ---------------------------------------------------------------- 3 GC 停顿
  // 同一个堆上限 256MB；8M 次分配、1/8 存活。
  // HotSpot 的 gc 日志走 %p（每次跑一个文件，免得 reps 之间互相覆盖）。
  c('gc', 'gc-hs-1', 'H', HS, ['-Xmx256m', '-Xlog:gc:file=D:/vmless/bench/logs/hs-gc-%p.log', '-cp', `${OUT}/Gc`, 'Gc'], { reps: 1, timeoutMs: 120000 }),
  c('gc', 'gc-hs-2', 'H', HS, ['-Xmx256m', '-Xlog:gc:file=D:/vmless/bench/logs/hs-gc-%p.log', '-cp', `${OUT}/Gc`, 'Gc'], { reps: 1, timeoutMs: 120000 }),
  c('gc', 'gc-hs-3', 'H', HS, ['-Xmx256m', '-Xlog:gc:file=D:/vmless/bench/logs/hs-gc-%p.log', '-cp', `${OUT}/Gc`, 'Gc'], { reps: 1, timeoutMs: 120000 }),
  c('gc', 'gc-vml-1', 'V', RJIT, [`${OUT}/Gc/Gc.class`], { reps: 1, timeoutMs: 300000, env: { VMLESS_GC_STATS: '1', VMLESS_HEAP_MB: '256' } }),
  c('gc', 'gc-vml-2', 'V', RJIT, [`${OUT}/Gc/Gc.class`], { reps: 1, timeoutMs: 300000, env: { VMLESS_GC_STATS: '1', VMLESS_HEAP_MB: '256' } }),
  c('gc', 'gc-vml-3', 'V', RJIT, [`${OUT}/Gc/Gc.class`], { reps: 1, timeoutMs: 300000, env: { VMLESS_GC_STATS: '1', VMLESS_HEAP_MB: '256' } }),
  // 解释器那条路：GC 无停顿统计，只测总量（很慢，1 次）
  c('gc', 'gc-vml-interp', 'V', RJDK, [`${OUT}/Gc`, 'Gc'], { reps: 1, timeoutMs: 300000 }),

  // ---------------------------------------------------------------- 5 类加载
  c('classload', 'hs-classload', 'H', HS, ['-cp', `${OUT}/ClassLoad2`, 'ClassLoad2'], { reps: 3, timeoutMs: 180000 }),
  c('classload', 'hs-empty-xshareoff', 'H', HS, ['-Xshare:off', '-cp', `${OUT}/Empty`, 'Empty'], { reps: 5, timeoutMs: 60000 }),
  c('classload', 'vml-jdk-load5421', 'V', RJDK, [`${OUT}/Empty`, 'Empty'], { reps: 3, timeoutMs: 120000 }),
  c('classload', 'vml-classload', 'V', RJDK, [`${OUT}/ClassLoad2`, 'ClassLoad2'], { reps: 1, timeoutMs: 300000 }),

  // ---------------------------------------------------------------- 6 解释执行吞吐
  c('interp', 'hs-p2', 'H', HS, ['-cp', `${OUT}/P2j`, 'P2j'], { reps: 3, timeoutMs: 120000 }),
  c('interp', 'vml-interp-p2j', 'V', RJDK, [`${OUT}/P2j`, 'P2j'], { reps: 2, timeoutMs: 300000 }),
  c('interp', 'vml-interp-p2', 'V', RJDK, [JMS, 'P2'], { reps: 2, timeoutMs: 300000 }),

  // ---------------------------------------------------------------- 7 JIT 吞吐
  c('jit', 'hs-p2j-jit', 'H', HS, ['-cp', `${OUT}/P2j`, 'P2j'], { reps: 5, timeoutMs: 120000 }),
  c('jit', 'vml-jit-p2j', 'V', RJIT, [`${OUT}/P2j/P2j.class`], { reps: 5, timeoutMs: 300000 }),
  c('jit', 'vml-jit-p2', 'V', RJIT, [`${JMS}/P2.class`], { reps: 2, timeoutMs: 300000 }),

  // ---------------------------------------------------------------- 8 并行扩展性
  c('parallel', 'hs-p1j-1', 'H', HS, ['-cp', `${OUT}/P1L1`, 'P1L1'], { reps: 2, timeoutMs: 120000 }),
  c('parallel', 'hs-p1j-2', 'H', HS, ['-cp', `${OUT}/P1L2`, 'P1L2'], { reps: 2, timeoutMs: 120000 }),
  c('parallel', 'hs-p1j-4', 'H', HS, ['-cp', `${OUT}/P1L4`, 'P1L4'], { reps: 2, timeoutMs: 120000 }),
  c('parallel', 'hs-p1j-8', 'H', HS, ['-cp', `${OUT}/P1L8`, 'P1L8'], { reps: 2, timeoutMs: 120000 }),
  c('parallel', 'vml-interp-p1j-1', 'V', RJDK, [`${OUT}/P1L1`, 'P1L1'], { reps: 2, timeoutMs: 300000 }),
  c('parallel', 'vml-interp-p1j-2', 'V', RJDK, [`${OUT}/P1L2`, 'P1L2'], { reps: 2, timeoutMs: 300000 }),
  c('parallel', 'vml-interp-p1j-4', 'V', RJDK, [`${OUT}/P1L4`, 'P1L4'], { reps: 2, timeoutMs: 300000 }),
  c('parallel', 'vml-interp-p1j-8', 'V', RJDK, [`${OUT}/P1L8`, 'P1L8'], { reps: 2, timeoutMs: 300000 }),
  c('parallel', 'vml-jit-p1j-4', 'V', RJIT, [`${OUT}/P1L4/P1L4.class`], { reps: 1, timeoutMs: 120000 }),
];
