package vmless;

/** bin/javac.exe：推导 java.home，并在未指定 --release/-source/--system 时默认 --release 25。 */
public final class Javac {
    public static void main(String[] args) throws Exception {
        Home.install();
        boolean hasPlatformOption = false;
        for (String a : args) {
            if (a.equals("--release") || a.equals("-source") || a.equals("--system")
                    || a.startsWith("--release=") || a.startsWith("--system=")) {
                hasPlatformOption = true;
                break;
            }
        }
        String[] effective;
        if (hasPlatformOption) {
            effective = args;
        } else {
            effective = new String[args.length + 2];
            effective[0] = "--release";
            effective[1] = "25";
            System.arraycopy(args, 0, effective, 2, args.length);
        }
        com.sun.tools.javac.Main.main(effective);
    }
}
