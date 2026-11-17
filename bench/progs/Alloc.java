// 纯分配压力：n 个 16B 级对象，全部立即死亡（测峰值 RSS 与分配吞吐）。
public class Alloc {
    public static void main(String[] a) {
        int n = Integer.parseInt(a.length > 0 ? a[0] : "8000000");
        long sum = 0;
        for (int i = 0; i < n; i++) {
            ANode o = new ANode(i);
            sum += o.v;
        }
        System.out.println(sum);
    }
}
class ANode {
    int v;
    long pad1;
    ANode(int v) { this.v = v; }
}
