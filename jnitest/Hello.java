public class Hello {
    static native void hi();
    static native int add(int a, int b);
    static native String echo(String s);
    static native int arrayTest(int[] a);
    static native void throwIt(String msg);
    static native int refTest(Object o);
    static native void invokeOn(Object o, int v);
    static native double invokeDouble(Object o, double v);
    static native void callback();
    static native int fieldPlay(Object o);

    static String cbResult = "";

    static void onCall(String s) {
        cbResult = s;
    }

    static class Box {
        int value = 1;
        void set(int v) { value = v; }
        double scale(double x) { return x * 2.5; }
    }

    public static void main(String[] args) {
        System.loadLibrary("hello");
        hi();
        System.out.println("add=" + add(2, 3));
        System.out.println("echo=" + echo("Hello, 中文"));
        int[] arr = {1, 2, 3, 4};
        int sum = arrayTest(arr);
        System.out.println("sum=" + sum + " first=" + arr[0] + " last=" + arr[3]);
        try {
            throwIt("boom 中文");
            System.out.println("no exception");
        } catch (RuntimeException e) {
            System.out.println("caught=" + e.getMessage());
        }
        long last = 0;
        for (int i = 0; i < 100; i++) last = refTest(new Object());
        System.out.println("globalRefs=" + last);
        Box b = new Box();
        invokeOn(b, 42);
        System.out.println("box=" + b.value);
        System.out.println("scale=" + invokeDouble(b, 2.0));
        callback();
        System.out.println("cb=" + cbResult);
        System.out.println("fieldOld=" + fieldPlay(b) + " fieldNew=" + b.value);
    }
}
