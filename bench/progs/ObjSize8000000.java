public class ObjSize8000000 {
    public static void main(String[] a) {
        int n = 8000000;
        ONode[] arr = new ONode[n];
        for (int i = 0; i < n; i++) arr[i] = new ONode(i);
        System.out.println(arr[n - 1].v);
    }
}
class ONode { int v; long pad1; ONode(int v) { this.v = v; } }
