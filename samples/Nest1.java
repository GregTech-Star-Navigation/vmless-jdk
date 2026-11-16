// 静态嵌套类：类型名按 javac 的二进制名 `Outer$Inner` 走
class Nest1 {
    static class Inner {
        int v;

        Inner(int v) {
            this.v = v;
        }

        int twice() {
            return v * 2;
        }
    }

    static class Pair {
        Inner a;
        Inner b;

        Pair(Inner a, Inner b) {
            this.a = a;
            this.b = b;
        }

        int sum() {
            return a.twice() + b.twice();
        }
    }

    static int helper(int x) {
        return x + 100;
    }

    public static void main(String[] args) {
        Inner i = new Inner(7);
        System.out.println(i.v);
        System.out.println(i.twice());
        // 另一种写法：从外层类名点进来
        Nest1.Inner j = new Nest1.Inner(9);
        System.out.println(j.twice());
        Pair p = new Pair(i, j);
        System.out.println(p.sum());
        System.out.println(Nest1.helper(5));
    }
}
