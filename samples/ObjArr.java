class ObjArr {
    int x;
    ObjArr(int v) { this.x = v; }
    static int run(int n) {
        int[] xs = new int[100];
        int i = 0;
        while (i < 100) { xs[i] = i * 3; i = i + 1; }
        ObjArr o = new ObjArr(xs[50]);
        int s = 0;
        i = 0;
        while (i < n) { s = s + o.x + xs[i]; i = i + 1; }
        return s;
    }
    public static void main(String[] a) { System.out.println(run(100)); }
}
