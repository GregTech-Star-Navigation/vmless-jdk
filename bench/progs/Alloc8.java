// 8,000,000 次分配、对象全部立刻死亡（JIT 友好：无参数、无 indy）
public class Alloc8 {
    public static void main(String[] a) {
        long sum = 0;
        for (int i = 0; i < 8000000; i++) {
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
