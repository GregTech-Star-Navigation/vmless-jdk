public class P {
    public static void main(String[] a) {
        System.out.println("java.home=" + System.getProperty("java.home"));
        System.out.println("os.name=" + System.getProperty("os.name"));
        System.out.println("props=" + (System.getProperties() == null ? "null" : "ok"));
    }
}
