// 内联场景 2：被调方体内有 if/else 且两条分支各自 return（考地址表 + return 重定位）
public class InlBranch {
    static int sign(int v) {
        if (v < 0) return -1;
        else if (v > 0) return 1;
        return 0;
    }
    static int pick(int a, int b, boolean f) {
        if (f) { return a * 2 + b; }
        return b - a;
    }
    public static void main(String[] x) {
        int[] vs = { -9, -1, 0, 1, 9 };
        for (int i = 0; i < vs.length; i++) System.out.println(sign(vs[i]));
        System.out.println(pick(3, 4, true));
        System.out.println(pick(3, 4, false));
    }
}
