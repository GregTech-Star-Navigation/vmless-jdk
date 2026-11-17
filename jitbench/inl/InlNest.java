// 内联场景 5：深度 2 —— main 调 mid，mid 体内再调 leaf
public class InlNest {
    static int leaf(int a) { return a * a + 1; }
    static int mid(int a, int b) { return leaf(a) + leaf(b); }
    static long top(long v) { return v + leaf((int) (v % 7)); }
    public static void main(String[] x) {
        System.out.println(mid(3, 4));
        System.out.println(top(100L));
        int s = 0;
        for (int i = 0; i < 5; i++) s += mid(i, i + 1);
        System.out.println(s);
    }
}
