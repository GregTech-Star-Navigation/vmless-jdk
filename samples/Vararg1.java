// 可变参数：`String... args` —— 声明侧是 String[]，调用点把尾部实参打包成数组
class Vararg1 {
    static void show(String tag, String... items) {
        System.out.println(tag);
        System.out.println(items.length);
        for (String it : items) {
            System.out.println(it);
        }
    }

    static int count(String... items) {
        return items.length;
    }

    static String join(String sep, String... items) {
        String acc = "";
        for (String it : items) {
            if (acc == "") {
                acc = it;
            } else {
                acc = acc + sep + it;
            }
        }
        return acc;
    }

    public static void main(String... args) {
        show("two", "a", "b");
        show("one", "x");
        show("none");
        // 也可以直接传数组（不再包一层）
        String[] arr = new String[2];
        arr[0] = "p";
        arr[1] = "q";
        show("array", arr);
        System.out.println(count());
        System.out.println(count("z"));
        System.out.println(join("-", "u", "v", "w"));
        System.out.println(join("+"));
        System.out.println(args.length);
    }
}
