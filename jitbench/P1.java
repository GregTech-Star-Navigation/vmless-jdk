public class P1 {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += (i * 31 + 7) % 1009;
        return s;
    }
    public static void main(String[] a) throws Exception {
        int nt = 4;
        long n = 8_000_000L;
        long t0 = System.nanoTime();
        Thread[] ts = new Thread[nt];
        final long[] res = new long[nt];
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
