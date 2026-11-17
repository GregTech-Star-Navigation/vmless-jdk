// 内联场景 7：不该内联的三种（递归 / 有异常表 / 引用参数），行为必须照旧正确
// 注：`guarded(0)`（除数 0 → ArithmeticException）在本项目里是**既有缺口**
//     （解释器直接报「整数除零」退出），与内联无关，所以样例里只用 guarded(4)。
public class InlReject {
    static int fib(int n) { if (n < 2) return n; return fib(n - 1) + fib(n - 2); }
    static int guarded(int n) {
        try { return 100 / n; } catch (ArithmeticException e) { return -1; }
    }
    static String tag(String s) { return s + "!"; }
    public static void main(String[] x) {
        System.out.println(fib(15));
        System.out.println(guarded(4));
        System.out.println(tag("hi"));
    }
}
