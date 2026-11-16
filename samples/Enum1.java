// 枚举基础：values()/valueOf()/ordinal()/name()/== 比较
enum EnumColor {
    RED, GREEN, BLUE
}

class Enum1 {
    public static void main(String[] args) {
        EnumColor c = EnumColor.RED;
        System.out.println(c.name());
        System.out.println(c.ordinal());
        System.out.println(c == EnumColor.RED);
        System.out.println(c != EnumColor.RED);
        System.out.println(EnumColor.GREEN.ordinal());
        EnumColor[] vs = EnumColor.values();
        System.out.println(vs.length);
        System.out.println(vs[0].name());
        System.out.println(vs[2].ordinal());
        EnumColor d = EnumColor.valueOf("GREEN");
        System.out.println(d.name());
        System.out.println(d.ordinal());
        System.out.println(d == EnumColor.GREEN);
        System.out.println(EnumColor.valueOf("nope") == null);
    }
}
