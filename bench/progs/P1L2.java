// P1 的 lambda 版（与 jdkms/par/P1.java 同一形状），线程数写死为 2。
public class P1L2 {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += (i * 31 + 7) % 1009;
        return s;
    }
    public static void main(String[] a) throws Exception {
        int nt = 2;
        long n = 8_000_000L;
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
        for (int j = 0; j < nt; j++) sum += res[j];
        System.out.println(sum);
        System.out.println((t1 - t0) / 1000000L);
    }
}
