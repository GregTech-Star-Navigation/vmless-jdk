// R5：OOP（继承/接口/多态/异常/内部类/枚举/泛型方法）
public class R5 {
    interface Shape { double area(); default String tag() { return "shape"; } }
    static abstract class Base implements Shape {
        final String name;
        Base(String n) { name = n; }
        public String toString() { return name + ":" + (int) area(); }
    }
    static class Circle extends Base {
        final double r; Circle(double r) { super("circle"); this.r = r; }
        public double area() { return 3.14159 * r * r; }
        public String tag() { return "round"; }
    }
    static class Rect extends Base {
        final double w, h; Rect(double w, double h) { super("rect"); this.w = w; this.h = h; }
        public double area() { return w * h; }
    }
    enum Color { RED, GREEN, BLUE;
        int weight() { return ordinal() + 1; }
    }
    static <T extends Comparable<T>> T max(T[] arr) {
        T best = arr[0];
        for (T t : arr) if (t.compareTo(best) > 0) best = t;
        return best;
    }
    static class Res implements AutoCloseable {
        final String id; Res(String id) { this.id = id; System.out.println("open " + id); }
        public void close() { System.out.println("close " + id); }
    }
    public static void main(String[] a) {
        Shape[] ss = { new Circle(2), new Rect(3, 4), new Circle(0.5) };
        double total = 0;
        StringBuilder sb = new StringBuilder();
        for (Shape s : ss) { total += s.area(); sb.append(s).append('|').append(s.tag()).append(';'); }
        System.out.println(sb.toString());
        System.out.println("total=" + (int) total);

        for (Color c : Color.values()) System.out.print(c + "" + c.weight() + " ");
        System.out.println();

        System.out.println("max=" + max(new Integer[]{3, 9, 2, 7}));

        // 异常层次
        try {
            try { throw new IllegalStateException("inner"); }
            finally { System.out.println("finally1"); }
        } catch (RuntimeException e) {
            System.out.println("caught " + e.getMessage());
        }
        try (Res r1 = new Res("A"); Res r2 = new Res("B")) {
            System.out.println("body");
        }
        int depth = 0;
        try { depth = deep(0); } catch (StackOverflowError e) { depth = -1; }
        System.out.println("recursion=" + depth);
        System.out.println("done");
    }
    static int deep(int n) { if (n == 5000) return n; return deep(n + 1); }
}
