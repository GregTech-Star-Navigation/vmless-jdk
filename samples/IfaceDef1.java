// 接口 default / static 方法：实现类不覆写时落到 default 实现
interface Shape {
    int area();

    default int twice() {
        return area() * 2;
    }

    default int triple() {
        return area() * 3;
    }

    static int unit() {
        return 1;
    }
}

class Sq implements Shape {
    int s;

    Sq(int s) {
        this.s = s;
    }

    public int area() {
        return s * s;
    }

    public int triple() {
        return 99;
    }
}

class IfaceDef1 {
    public static void main(String[] args) {
        Shape a = new Sq(3);
        System.out.println(a.area());
        System.out.println(a.twice());
        System.out.println(a.triple());
        System.out.println(Shape.unit());
        Sq b = new Sq(4);
        System.out.println(b.twice());
        System.out.println(b.triple());
    }
}
