// 内联场景 8：内联体里带常数的除/余（顺带压 divmod 的边界）
public class InlEdge {
    static int div9(int v) { return v / 9; }
    static int rem9(int v) { return v % 9; }
    static long lrem(long v) { return v % 1009; }
    static int remNeg(int v) { return v % -7; }
    static long ldiv(long v) { return v / -1009; }
    public static void main(String[] x) {
        int[] vs = { 0, 1, -1, 8, 9, 10, -8, -9, -10, 2147483647, -2147483648 };
        for (int i = 0; i < vs.length; i++) {
            System.out.println(div9(vs[i]));
            System.out.println(rem9(vs[i]));
            System.out.println(remNeg(vs[i]));
        }
        long[] ls = { 0L, 1L, -1L, 1008L, 1009L, -1009L, 9223372036854775807L, -9223372036854775808L };
        for (int i = 0; i < ls.length; i++) {
            System.out.println(lrem(ls[i]));
            System.out.println(ldiv(ls[i]));
        }
    }
}
