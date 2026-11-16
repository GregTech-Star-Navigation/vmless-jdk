class Loop {
    static int sum(int n) {
        int i = 0;
        int s = 0;
        while (i < n) {
            s = s + i;
            i = i + 1;
        }
        return s;
    }
    static int max(int a, int b) {
        if (a > b) { return a; }
        return b;
    }
    public static void main(String[] args) {
        System.out.println(sum(5));
        System.out.println(max(7, 3));
    }
}
