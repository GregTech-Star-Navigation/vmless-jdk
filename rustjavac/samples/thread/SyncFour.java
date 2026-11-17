// 跨线程 synchronized 的回归样例（4 线程版）：4 × 10 万次抢同一把锁。
//
// 期望输出：`count=400000` 与 `true`
//
// 两个断言各有分工：
// * `count=400000` —— 互斥语义没丢（丢一次就少一）
// * `true`        —— 主线程读到的值就是最终值（`join` 的可见性没被吃掉）
public class SyncFour {
    static int count = 0;
    static final Object LOCK = new Object();

    static void bump() {
        synchronized (LOCK) {
            count++;
        }
    }

    public static void main(String[] a) throws Exception {
        int nt = 4, ops = 100_000;
        Thread[] ts = new Thread[nt];
        for (int i = 0; i < nt; i++) {
            ts[i] = new Thread(() -> {
                for (int k = 0; k < ops; k++) bump();
            });
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        System.out.println("count=" + count);
        System.out.println(count == nt * ops);
    }
}
