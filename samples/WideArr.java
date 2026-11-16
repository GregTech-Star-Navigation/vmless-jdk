class WideArr {
    public static void main(String[] args) {
        long[] xs = new long[3];
        xs[1] = 5000000000L;
        xs[2] = xs[1] + 1L;
        System.out.println(xs[0]);
        System.out.println(xs[1]);
        System.out.println(xs[2]);
        System.out.println(xs.length);
        long s = 0L;
        int i = 0;
        while (i < xs.length) {
            s = s + xs[i];
            i = i + 1;
        }
        System.out.println(s);
        double[] ds = new double[2];
        ds[0] = 1.5;
        ds[1] = ds[0] * 2.0;
        System.out.println(ds[0]);
        System.out.println(ds[1]);
        System.out.println(ds[0] + ds[1] + (double) s);
        int[] is = new int[2];
        is[0] = 7;
        is[1] = is[0] * 3;
        System.out.println(is[0] + is[1]);
    }
}
