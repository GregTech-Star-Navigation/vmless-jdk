// switch on String：编成 equals 链（语义与 javac 的 hashCode+equals 一致）
class SwitchStr1 {
    static int code(String s) {
        switch (s) {
            case "a":
                return 1;
            case "bb":
                return 2;
            case "ccc":
            case "dddd":
                return 3;
            default:
                return 0;
        }
    }

    public static void main(String[] args) {
        System.out.println(code("a"));
        System.out.println(code("bb"));
        System.out.println(code("ccc"));
        System.out.println(code("dddd"));
        System.out.println(code("e"));

        String v = "bb";
        switch (v) {
            case "a":
                System.out.println("A");
                break;
            case "bb":
                System.out.println("B");
                break;
            default:
                System.out.println("D");
        }

        // 没有 default 的 switch
        switch (v) {
            case "zz":
                System.out.println("Z");
                break;
        }

        // 选择子是方法调用结果
        switch (code("a") == 1 ? "one" : "other") {
            case "one":
                System.out.println("is one");
                break;
            default:
                System.out.println("other");
        }
        System.out.println("done");
    }
}
