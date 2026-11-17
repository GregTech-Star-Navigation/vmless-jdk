// 内联场景 3：被调方体内有循环（后向 goto，负偏移）
public class InlLoop {
    static int sumTo(int n) {
        int s = 0;
        int i = 0;
        while (i < n) { s += i; i++; }
        return s;
    }
    static long pow2(int n) {
        long r = 1;
        for (int i = 0; i < n; i++) r = r * 2;
        return r;
    }
    public static void main(String[] x) {
        System.out.println(sumTo(10));
        System.out.println(sumTo(0));
        System.out.println(pow2(20));
        System.out.println(pow2(0));
        int t = 0;
        for (int k = 0; k < 4; k++) t += sumTo(k);
        System.out.println(t);
    }
}
