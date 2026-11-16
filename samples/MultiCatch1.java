// 多重捕获 `catch (A | B e)`：同一段 handler，异常表里两条 catch_type
class MultiCatch1 {
    static void run(int which) {
        try {
            if (which == 0) {
                throw new IllegalStateException("is");
            } else if (which == 1) {
                throw new ArithmeticException("ae");
            }
            System.out.println("no throw");
        } catch (IllegalStateException | ArithmeticException e) {
            System.out.println("caught both");
        } finally {
            System.out.println("finally");
        }
    }

    static void rethrow(boolean flag) {
        try {
            if (flag) {
                throw new IllegalStateException("x");
            }
            throw new ArithmeticException("y");
        } catch (IllegalStateException | ArithmeticException e) {
            System.out.println("inner");
            throw e;
        }
    }

    public static void main(String[] args) {
        run(0);
        run(1);
        run(2);
        try {
            rethrow(true);
        } catch (IllegalStateException | ArithmeticException e) {
            System.out.println("outer");
        }
        try {
            rethrow(false);
        } catch (RuntimeException e) {
            System.out.println("single");
        }
    }
}
