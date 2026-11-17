// P2 的 JIT 友好版：同样的工作量，不含 invokedynamic。
public class P2j {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += (i * 31 + 7) % 1009;
        return s;
    }
    public static void main(String[] a) {
        long n = 8_000_000L;
        long t0 = System.nanoTime();
        long s = 0;
        for (int k = 0; k < 4; k++) s += work(n);
        long t1 = System.nanoTime();
        System.out.println(s);
        System.out.println((t1 - t0) / 1000000L);
    }
}
