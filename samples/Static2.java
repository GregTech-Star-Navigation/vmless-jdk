class Counter {
    static int n;
    static long total = 0L;
    static double avg = 0.5;
    static String tag = "c";

    static {
        System.out.println("init Counter");
    }

    static void add(int v) {
        n = n + 1;
        total = total + v;
    }

    static long get() {
        return total;
    }

    static double dbl() {
        return avg;
    }
}

class SubCounter extends Counter {
    static int own = 2;
}

class Static2 {
    static {
        System.out.println("init Static2");
    }

    public static void main(String[] args) {
        System.out.println(Counter.tag);
        for (int i = 1; i <= 4; i = i + 1) {
            Counter.add(i);
        }
        System.out.println(Counter.n);
        System.out.println(Counter.get());
        System.out.println(Counter.dbl());
        System.out.println(SubCounter.own);
        System.out.println(SubCounter.n);
        Counter.n = 50;
        System.out.println(SubCounter.n);
    }
}
