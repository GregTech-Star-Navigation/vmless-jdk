class P {
    int x;
    P(int v) { this.x = v; }
    int get() { return x; }
    int twice() { return x * 2; }
}
class Q extends P {
    int y;
    Q() { super(7); y = 3; }
    int sum() { return get() + y; }
}
class InhBasic {
    public static void main(String[] args) {
        Q q = new Q();
        System.out.println(q.x);
        System.out.println(q.y);
        System.out.println(q.get());
        System.out.println(q.twice());
        System.out.println(q.sum());
        P p = new P(11);
        System.out.println(p.get());
    }
}
