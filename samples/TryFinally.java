public class TryFinally {
    static int f(int m) {
        try {
            if (m == 0) {
                System.out.println("body0");
                return 1;
            }
            if (m == 1) {
                throw new RuntimeException("boom");
            }
            System.out.println("body2");
            return 2;
        } catch (RuntimeException e) {
            System.out.println("caught");
            return 3;
        } finally {
            System.out.println("finally");
        }
    }
    public static void main(String[] args) {
        System.out.println(f(0));
        System.out.println(f(1));
        System.out.println(f(2));
    }
}
