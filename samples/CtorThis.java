class CtorThis {
    int a;
    int b;
    int sum;

    CtorThis() {
        this(7, 8);
    }

    CtorThis(int a) {
        this(a, a + 1);
    }

    CtorThis(int a, int b) {
        this.a = a;
        this.b = b;
        this.sum = a + b;
    }

    public static void main(String[] args) {
        CtorThis x = new CtorThis();
        System.out.println(x.a);
        System.out.println(x.b);
        System.out.println(x.sum);
        CtorThis y = new CtorThis(3);
        System.out.println(y.a);
        System.out.println(y.b);
        System.out.println(y.sum);
        CtorThis z = new CtorThis(2, 9);
        System.out.println(z.sum);
    }
}
