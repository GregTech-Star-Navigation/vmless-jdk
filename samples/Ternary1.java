class Ternary1 {
    static int pick(boolean c, int a, int b) {
        return c ? a : b;
    }

    static long wpick(boolean c, int a, long b) {
        return c ? a : b;
    }

    static double dpick(boolean c, int a, double b) {
        return c ? a : b;
    }

    static String spick(boolean c, String a, String b) {
        return c ? a : b;
    }

    static String npick(boolean c) {
        return c ? "yes" : null;
    }

    public static void main(String[] args) {
        int x = 3;
        int y = 8;
        System.out.println(x < y ? x : y);
        System.out.println(x > y ? x : y);
        System.out.println(pick(x < y, 100, 200));
        System.out.println(wpick(true, 7, 10000000000L));
        System.out.println(wpick(false, 7, 10000000000L));
        System.out.println(dpick(true, 1, 2.5));
        System.out.println(dpick(false, 1, 2.5));

        String a = "aa";
        String b = "bb";
        System.out.println(spick(true, a, b));
        System.out.println(spick(false, a, b));
        System.out.println(spick(true, null, b));
        System.out.println(npick(true));
        System.out.println(npick(false));

        // 嵌套（含当实参、当条件的一部分）
        boolean big = x < y;
        System.out.println(big ? (x > 0 ? "both" : "neg") : "small");
        System.out.println(pick(x > 0 ? x < y : false, 1, 2));
        System.out.println((big ? x : y) + 1);
        System.out.println(big ? true : false);
        boolean fb = big ? true : false;
        System.out.println(fb);

        // 三元当数组下标 / 位移量
        int[] arr = new int[4];
        arr[0] = 5;
        arr[1] = 6;
        System.out.println(arr[big ? 1 : 0]);
        System.out.println(1 << (big ? 2 : 3));
    }
}
