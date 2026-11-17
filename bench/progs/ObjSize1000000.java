// 保持 n 个对象存活，用来量「每个对象占多少字节」（斜率法）
public class ObjSize1000000 {
    public static void main(String[] a) {
        int n = 1000000;
        ONode[] arr = new ONode[n];
        for (int i = 0; i < n; i++) arr[i] = new ONode(i);
        System.out.println(arr[n - 1].v);
    }
}
class ONode {
    int v;
    long pad1;
    ONode(int v) { this.v = v; }
}
