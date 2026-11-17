import java.lang.reflect.Method;

public class P2 {
    public static void main(String[] a) throws Exception {
        Method m = Class.forName("java.lang.System").getDeclaredMethod("initPhase1");
        m.setAccessible(true);
        m.invoke(null);
        System.out.println("java.home=" + System.getProperty("java.home"));
        System.out.println("os.name=" + System.getProperty("os.name"));
    }
}
