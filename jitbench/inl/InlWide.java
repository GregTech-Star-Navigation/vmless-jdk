// 内联场景 4：long / float / double 参数与返回值（两槽形参的落槽顺序）
public class InlWide {
    static double scale(double v, int k) { return v * k; }
    static float fadd(float a, float b) { return a + b; }
    static long blend(long a, double b) { return a + (long) b; }
    static double mix(int a, long b, float c, double d) { return a + b + c + d; }
    public static void main(String[] x) {
        System.out.println(scale(1.5, 4));
        System.out.println(fadd(1.25f, 2.5f));
        System.out.println(blend(100L, 2.75));
        System.out.println(mix(1, 2L, 0.5f, 0.25));
    }
}
