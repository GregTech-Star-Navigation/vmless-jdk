// P1 的 JIT 友好版：线程数由参数给（默认 4），不用 lambda。
public class P1j4 {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += (i * 31 + 7) % 1009;
        return s;
    }
    static long[] res;
    static class W extends Thread {
        int k;
        long n;
        W(int k, long n) { this.k = k; this.n = n; }
        public void run() { res[k] = work(n); }
    }
    public static void main(String[] a) throws Exception {
        int nt = Integer.parseInt("4");
        long n = 8_000_000L;
        res = new long[nt];
        Thread[] ts = new Thread[nt];
        long t0 = System.nanoTime();
        for (int i = 0; i < nt; i++) ts[i] = new W(i, n);
        for (int i = 0; i < nt; i++) ts[i].start();
        for (int i = 0; i < nt; i++) ts[i].join();
        long t1 = System.nanoTime();
        long sum = 0;
        for (int i = 0; i < nt; i++) sum += res[i];
        System.out.println(sum);
        System.out.println((t1 - t0) / 1000000L);
    }
}
