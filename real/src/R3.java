import java.util.*; import java.util.concurrent.*; import java.util.concurrent.atomic.*;
// R3：并发（线程池 + 原子量 + 并发容器 + Callable/Future + CountDownLatch）
public class R3 {
    public static void main(String[] a) throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(4);
        AtomicLong total = new AtomicLong();
        List<Future<Long>> fs = new ArrayList<>();
        for (int t = 0; t < 8; t++) {
            final int id = t;
            fs.add(pool.submit(new Callable<Long>() {
                public Long call() {
                    long s = 0;
                    for (int i = 0; i < 200000; i++) s += (i * 31L + id) % 1009;
                    return s;
                }
            }));
        }
        for (Future<Long> f : fs) total.addAndGet(f.get());
        pool.shutdown();
        pool.awaitTermination(10, TimeUnit.SECONDS);
        System.out.println("total=" + total.get());

        ConcurrentHashMap<String, AtomicInteger> m = new ConcurrentHashMap<>();
        CountDownLatch start = new CountDownLatch(1), done = new CountDownLatch(4);
        for (int t = 0; t < 4; t++) {
            final int id = t;
            new Thread(() -> {
                try { start.await(); } catch (Exception e) {}
                for (int i = 0; i < 5000; i++) m.computeIfAbsent("k" + (i % 50), k -> new AtomicInteger()).incrementAndGet();
                done.countDown();
            }).start();
        }
        start.countDown();
        done.await();
        int sum = 0;
        for (AtomicInteger v : m.values()) sum += v.get();
        System.out.println("keys=" + m.size() + " cnt=" + sum);
        System.out.println("done");
    }
}
