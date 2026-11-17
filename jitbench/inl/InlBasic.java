// 内联场景 1：小静态方法 + 两个调用点
public class InlBasic {
    static int add(int a, int b) { return a + b; }
    static long mulsub(long a, long b, long c) { return a * b - c; }
    public static void main(String[] x) {
        int s = 0;
        for (int i = 0; i < 5; i++) s = add(s, i);
        System.out.println(s);
        System.out.println(add(add(1, 2), add(3, 4)));
        System.out.println(mulsub(7L, 6L, 5L));
        System.out.println(mulsub(-7L, 6L, 5L));
    }
}
