class P {
    int x;
    P(int v) { this.x = v; }
    int get() { return x; }
    int tag() { return 1; }
}
class Q extends P {
    Q(int v) { super(v); }
    int get() { return x * 10; }
    int tag() { return 2; }
}
class InhOverride {
    public static void main(String[] args) {
        P p = new Q(3);
        System.out.println(p.get());
        System.out.println(p.tag());
        System.out.println(((P) new Q(4)).get());
        P r = new P(5);
        System.out.println(r.get());
        System.out.println(r.tag());
        Q q = new Q(6);
        System.out.println(q.get());
    }
}
