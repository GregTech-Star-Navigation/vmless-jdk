class Sw {
    static int classify(int n) {
        switch (n) {
            case 1: return 10;
            case 2: return 20;
            case 3: return 30;
            default: return -1;
        }
    }
    public static void main(String[] args) {
        System.out.println(classify(1));
        System.out.println(classify(2));
        System.out.println(classify(9));
    }
}
