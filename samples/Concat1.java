class Concat1 {
    public static void main(String[] args) {
        int i = 42;
        long l = 10000000000L;
        double d = 2.5;
        boolean b = true;
        String s = "s";
        String nil = null;

        System.out.println("a" + i + "c");
        System.out.println("i=" + i);
        System.out.println("l=" + l);
        System.out.println("d=" + d);
        System.out.println("b=" + b);
        System.out.println("s=" + s);
        System.out.println("null=" + nil);
        System.out.println("mix " + i + " " + l + " " + d + " " + b + " " + s);
        System.out.println(i + " + " + i + " = " + (i + i));
        System.out.println("only" + " strings");
        String t = "x" + i + "y";
        System.out.println(t);
        System.out.println("neg " + (0 - i) + " big " + (l * 3));
        System.out.println("bool " + (i > 0) + " " + (i < 0));
    }
}
