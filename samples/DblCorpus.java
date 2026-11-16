class DblCorpus {
    public static void main(String[] args) {
        double x = 3.0;
        int i = 0;
        while (i < 60) {
            System.out.println(x);
            x = x * 1.7 + 0.013;
            i = i + 1;
        }
        double y = 0.1;
        i = 0;
        while (i < 40) {
            System.out.println(y);
            y = y / 3.0;
            i = i + 1;
        }
    }
}
