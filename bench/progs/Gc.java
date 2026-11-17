// 8,000,000 次分配，1/8 存活 —— 给 GC 造成保存与晋升压力。
// 刻意不用字符串拼接（javac 会编成 invokedynamic，JIT 不支持）。
public class Gc {
    static GNode[] keep;
    public static void main(String[] a) {
        int n = 8_000_000;
        keep = new GNode[n / 8];
        long t0 = System.nanoTime();
        int k = 0;
        for (int i = 0; i < n; i++) {
            GNode o = new GNode(i);
            if ((i & 7) == 0) { keep[k] = o; k++; }
        }
        long t1 = System.nanoTime();
        System.out.println(k);
        System.out.println((t1 - t0) / 1000000L);
        System.out.println(keep[keep.length - 1].v);
    }
}
class GNode {
    int v;
    long pad1;
    GNode(int v) { this.v = v; }
}
