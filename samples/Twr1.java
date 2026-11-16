// try-with-resources：糖化成 try/finally + close()（关闭顺序与声明反序）
class Res implements AutoCloseable {
    String name;

    Res(String name) {
        this.name = name;
        System.out.println("open " + name);
    }

    public void close() {
        System.out.println("close " + name);
    }

    void use() {
        System.out.println("use " + name);
    }
}

class Twr1 {
    public static void main(String[] args) {
        try (Res r = new Res("a")) {
            r.use();
        }

        try (Res x = new Res("x"); Res y = new Res("y")) {
            x.use();
            y.use();
        }

        // 异常路径：finally 里先关闭，再走 catch
        try {
            try (Res w = new Res("w")) {
                w.use();
                throw new RuntimeException("boom");
            }
        } catch (RuntimeException e) {
            System.out.println("boom caught");
        }

        // 资源 + 用户 finally 的顺序：先 close，再 finally 体
        try {
            try (Res f = new Res("f")) {
                f.use();
            } finally {
                System.out.println("user finally");
            }
        } catch (RuntimeException e) {
            System.out.println("never");
        }
        System.out.println("done");
    }
}
