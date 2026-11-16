// 泛型类 + 泛型方法 + 菱形 `<>`：擦除到上界（这里都是 Object）
class GenBox<T> {
    T v;

    GenBox(T v) {
        this.v = v;
    }

    T get() {
        return v;
    }

    void set(T nv) {
        this.v = nv;
    }
}

class GenPair<A, B> {
    A a;
    B b;

    GenPair(A a, B b) {
        this.a = a;
        this.b = b;
    }

    A first() {
        return a;
    }

    B second() {
        return b;
    }
}

public class Gen1 {
    // 方法级类型参数
    static <T> T pick(T a, T b, boolean wantFirst) {
        if (wantFirst) {
            return a;
        }
        return b;
    }

    static <T> void echo(T x) {
        System.out.println("echo");
        System.out.println(x);
    }

    public static void main(String[] args) {
        GenBox<String> b = new GenBox<>("x");
        // 擦除后 get() 是 Object，赋给 String 时要 checkcast
        String s = b.get();
        System.out.println(s);
        b.set("y");
        String s2 = b.get();
        System.out.println(s2);

        GenPair<String, String> p = new GenPair<>("k", "v");
        String k = p.first();
        String v = p.second();
        System.out.println(k);
        System.out.println(v);

        String picked = pick("a", "b", true);
        System.out.println(picked);
        String picked2 = pick("a", "b", false);
        System.out.println(picked2);

        echo("hi");

        // 上界就是 Object：直接当 Object 用也行
        Object o = b.get();
        System.out.println(o);
        GenBox<Object> bo = new GenBox<>("raw");
        Object o2 = bo.get();
        System.out.println(o2);
    }
}
