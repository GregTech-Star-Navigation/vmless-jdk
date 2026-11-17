// 只测 P1 的线程开销：work 的循环次数换成 1
public class P1o {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += (i * 31 + 7) % 1009;
        return s;
    }
    public static void main(String[] a) throws Exception {
        int nt = 4;
        long n = 1L;
        Thread[] ts = new Thread[nt];
        final long[] res = new long[nt];
        long t0 = System.nanoTime();
        for (int i = 0; i < nt; i++) {
            final int k = i;
            ts[i] = new Thread(() -> { res[k] = work(n); });
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long t1 = System.nanoTime();
        long sum = 0;
        for (long r : res) sum += r;
        System.out.println("sum=" + sum + " ms=" + ((t1 - t0) / 1_000_000));
    }
}
