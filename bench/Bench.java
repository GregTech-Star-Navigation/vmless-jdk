public class Bench {
    static final class Node {
        int v;
        Node next;
    }
    static int count(Node n) { int c = 0; while (n != null) { c++; n = n.next; } return c; }

    public static void main(String[] args) {
        int n = Integer.parseInt(args.length > 0 ? args[0] : "8000000");
        long t0 = System.nanoTime();
        Node head = null;
        for (int i = 0; i < n; i++) {
            Node x = new Node();
            x.v = i;
            if ((i & 7) == 0) { x.next = head; head = x; }   // 1/8 保留
        }
        long t1 = System.nanoTime();
        int live = count(head);
        long used = Runtime.getRuntime().totalMemory() - Runtime.getRuntime().freeMemory();
        System.out.printf("GARBAGE_START%n");
        head = null;
        System.gc();
        long t2 = System.nanoTime();
        long used2 = Runtime.getRuntime().totalMemory() - Runtime.getRuntime().freeMemory();
        System.out.printf("alloc=%.1f ns/obj live=%d heap_live=%.1f MB after_gc=%.1f MB alloc_ms=%.1f gc_ms=%.1f%n",
                (t1 - t0) / (double) n, live, used / 1048576.0, used2 / 1048576.0,
                (t1 - t0) / 1e6, (t2 - t1) / 1e6);
    }
}
