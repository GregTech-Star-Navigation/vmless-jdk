// 内联场景 9：被调方用 dup/swap 玩栈，且返回前栈上有中间值
public class InlStack {
    static int dupish(int a) { int b = a; return b + b; }
    static long swappy(long a, long b) { return (a - b) + (b - a) + a * b; }
    static int cond(int a, int b) {
        int r = a > b ? a : b;
        return r * (a < b ? a : b);
    }
    public static void main(String[] x) {
        System.out.println(dupish(21));
        System.out.println(swappy(6L, 7L));
        System.out.println(cond(3, 9));
        System.out.println(cond(9, 3));
    }
}
