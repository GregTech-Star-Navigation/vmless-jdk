class ForEach1 {
    public static void main(String[] args) {
        int[] xs = new int[4];
        xs[0] = 3;
        xs[1] = 1;
        xs[2] = 4;
        xs[3] = 1;
        int s = 0;
        for (int v : xs) {
            System.out.println(v);
            s = s + v;
        }
        System.out.println(s);

        long[] ls = new long[3];
        ls[0] = 10000000000L;
        ls[1] = 2L;
        ls[2] = 3L;
        long t = 0L;
        for (long v : ls) {
            t = t + v;
        }
        System.out.println(t);

        double[] ds = new double[2];
        ds[0] = 1.5;
        ds[1] = 2.25;
        double d = 0.0;
        for (double v : ds) {
            d = d + v;
        }
        System.out.println(d);

        boolean[] bs = new boolean[3];
        bs[0] = true;
        bs[1] = false;
        bs[2] = true;
        int c = 0;
        for (boolean b : bs) {
            if (b) {
                c = c + 1;
            }
        }
        System.out.println(c);

        String[] names = new String[3];
        names[0] = "aa";
        names[1] = "bbb";
        names[2] = "c";
        for (String n : names) {
            System.out.println(n);
        }
    }
}
