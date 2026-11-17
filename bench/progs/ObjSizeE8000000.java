// 空对象（只有对象头）——量「对象头 + 数组槽」的成本
public class ObjSizeE8000000 {
    public static void main(String[] a) {
        int n = 8000000;
        ENode[] arr = new ENode[n];
        for (int i = 0; i < n; i++) arr[i] = new ENode();
        System.out.println(arr.length);
    }
}
class ENode { }
