import java.io.*; import java.nio.charset.StandardCharsets; import java.nio.file.*; import java.util.*;
// R2：文件 I/O + 编码 + 行处理 + 临时文件生命周期
public class R2 {
    public static void main(String[] a) throws Exception {
        Path dir = Paths.get("D:/vmless/real/tmp");
        Files.createDirectories(dir);
        Path f = dir.resolve("r2.txt");
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 200; i++) sb.append("line-").append(i).append('-').append(i * 31 % 97).append('\n');
        Files.write(f, sb.toString().getBytes(StandardCharsets.UTF_8));
        System.out.println("size=" + Files.size(f));

        int lines = 0; long sum = 0;
        try (BufferedReader r = Files.newBufferedReader(f, StandardCharsets.UTF_8)) {
            String s;
            while ((s = r.readLine()) != null) {
                lines++;
                int dash = s.lastIndexOf('-');
                sum += Integer.parseInt(s.substring(dash + 1));
            }
        }
        System.out.println("lines=" + lines + " sum=" + sum);

        List<String> all = Files.readAllLines(f, StandardCharsets.UTF_8);
        System.out.println("readAllLines=" + all.size() + " first=" + all.get(0) + " last=" + all.get(199));

        // 中文往返
        Path g = dir.resolve("r2u.txt");
        Files.writeString(g, "中文测试 abc 123\n第二行");
        String back = Files.readString(g, StandardCharsets.UTF_8);
        System.out.println("utf8 len=" + back.length() + " c0=" + (int) back.charAt(0) + " lines=" + back.split("\n").length);

        // 拷贝 + 比较
        Path h = dir.resolve("r2copy.txt");
        Files.copy(f, h, StandardCopyOption.REPLACE_EXISTING);
        System.out.println("copyEqual=" + (Files.mismatch(f, h) == -1L));

        Files.delete(h); Files.delete(g); Files.delete(f);
        System.out.println("deleted=" + !Files.exists(f));
        System.out.println("done");
    }
}
