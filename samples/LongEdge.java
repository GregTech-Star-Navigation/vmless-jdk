class LongEdge {
    static long maxv() { return 9223372036854775807L; }
    static long addOne(long v) { return v + 1L; }
    static long mul(long a, long b) { return a * b; }
    static long sub(long a, long b) { return a - b; }
    public static void main(String[] args) {
        long m = maxv();
        System.out.println(m);
        System.out.println(addOne(m));
        System.out.println(mul(3037000500L, 3037000500L));
        System.out.println(sub(0L, m));
        System.out.println(-m);
        System.out.println(addOne(-1L));
        System.out.println(1000000007L * 1000000007L);
        System.out.println((int) m);
        System.out.println((double) m);
        double nv = -3.99;
        System.out.println((long) 3.99);
        System.out.println((int) nv);
    }
}
