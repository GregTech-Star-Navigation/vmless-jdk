public class TryThrow {
    public static void main(String[] args) {
        System.out.println("before");
        try {
            throw new RuntimeException("boom");
        } catch (RuntimeException e) {
            System.out.println("caught");
        }
        System.out.println("after");
    }
}
