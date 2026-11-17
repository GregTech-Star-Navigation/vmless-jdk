// 内联场景 6：final 类的虚调用（单态），方法体不碰 this
public final class InlVirt {
    final int twice(int v) { return v + v; }
    final long off(long v) { return v - 1; }
    public static void main(String[] x) {
        InlVirt o = new InlVirt();
        int s = 0;
        for (int i = 0; i < 6; i++) s = s + o.twice(i);
        System.out.println(s);
        System.out.println(o.off(100L));
    }
}
