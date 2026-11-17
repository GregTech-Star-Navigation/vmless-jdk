// 跨线程 synchronized 的回归样例（1 线程版）：走 lambda + indy，锁竞争不激烈，
// 但足以暴露「解释器线程没登记就进 execute」这类卡死（会把内存吃到爆）。
//
// 期望输出：`count=1000`
public class SyncOne {
    static int count = 0;
    static final Object LOCK = new Object();

    static void bump() {
        synchronized (LOCK) {
            count++;
        }
    }

    public static void main(String[] a) throws Exception {
        Thread t = new Thread(() -> {
            for (int k = 0; k < 1000; k++) bump();
        });
        t.start();
        t.join();
        System.out.println("count=" + count);
    }
}
