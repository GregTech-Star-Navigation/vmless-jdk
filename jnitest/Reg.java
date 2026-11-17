public class Reg {
    static native int triple(int x);
    static native String greet(String name);

    public static void main(String[] args) {
        System.loadLibrary("reg");
        System.out.println("triple=" + triple(14));
        System.out.println("greet=" + greet("world"));
    }
}
