// 比较结果当值用：println(a == b)、boolean f = x > y、return a == b
class Cmp1 {
    static boolean gt(int a, int b) { return a > b; }
    static boolean eq(int a, int b) { return a == b; }

    public static void main(String[] args) {
        int a = 3;
        int b = 5;
        System.out.println(a == b);
        System.out.println(a != b);
        System.out.println(a < b);
        System.out.println(a <= b);
        System.out.println(a > b);
        System.out.println(a >= b);
        boolean f = a > b;
        System.out.println(f);
        boolean g = a < b;
        System.out.println(g);
        System.out.println(gt(b, a));
        System.out.println(gt(a, b));
        System.out.println(eq(a, a));
        long la = 7L;
        long lb = 9L;
        System.out.println(la < lb);
        System.out.println(la == lb);
        System.out.println(la >= lb);
        double da = 1.5;
        double db = 2.5;
        System.out.println(da >= db);
        System.out.println(da < db);
        String s = "hi";
        System.out.println(s == null);
        System.out.println(s != null);
        Cmp1 p = new Cmp1();
        System.out.println(p == null);
        System.out.println(p != null);
    }
}
