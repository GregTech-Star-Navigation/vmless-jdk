public class Q3 {
    static long work(long n) {
        long s = 0;
        for (long i = 0; i < n; i++) s += i;
        return s;
    }
    public static void main(String[] a) {
        long n = 8_000_000L;
        long t0 = System.nanoTime();
        long s = 0;
        for (int k = 0; k < 4; k++) s += work(n);
        long t1 = System.nanoTime();
        System.out.println("sum=" + s + " ms=" + ((t1 - t0) / 1_000_000));
    }
}
