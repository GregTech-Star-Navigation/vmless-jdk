import java.util.*;
import java.util.function.*;

// 真程序样张 1：集合 + 泛型 + 排序 + 流式处理 + 字符串
public class R1 {
    static final class Emp {
        final String name; final String dept; final int salary;
        Emp(String n, String d, int s) { name = n; dept = d; salary = s; }
        String name() { return name; } String dept() { return dept; } int salary() { return salary; }
    }

    public static void main(String[] a) {
        List<Emp> es = new ArrayList<>();
        String[] names = {"amy","bob","cid","dan","eve","fay","gus","hal"};
        String[] depts = {"dev","ops","dev","qa","ops","dev","qa","ops"};
        for (int i = 0; i < names.length; i++)
            es.add(new Emp(names[i], depts[i], 100 + i * 37));

        // 分组求和（TreeMap 保证顺序）
        Map<String, Integer> byDept = new TreeMap<>();
        for (Emp e : es) byDept.merge(e.dept(), e.salary(), Integer::sum);
        System.out.println("byDept=" + byDept);

        // 排序（Comparator 链）
        es.sort(Comparator.comparingInt(Emp::salary).reversed().thenComparing(Emp::name));
        StringBuilder sb = new StringBuilder("top3=");
        for (int i = 0; i < 3; i++) { if (i > 0) sb.append(','); sb.append(es.get(i).name()); }
        System.out.println(sb);

        // 去重 + 统计
        Set<String> ds = new HashSet<>(Arrays.asList(depts));
        System.out.println("depts=" + new TreeSet<>(ds).size() + " avg="
            + es.stream().mapToInt(Emp::salary).average().orElse(0));

        // 手写哈希表（碰撞 + 扩容）
        Map<Integer, String> m = new HashMap<>();
        for (int i = 0; i < 5000; i++) m.put(i * 7919, "v" + i);
        int hit = 0;
        for (int i = 0; i < 5000; i++) if (m.get(i * 7919) != null) hit++;
        System.out.println("hash hit=" + hit + " size=" + m.size());

        // 字符串处理
        String s = "the quick brown fox jumps over the lazy dog";
        Map<Character, Integer> freq = new TreeMap<>();
        for (char c : s.toCharArray()) if (c != ' ') freq.merge(c, 1, Integer::sum);
        System.out.println("freq=" + freq.get('o') + "," + freq.get('t') + " words=" + s.split(" ").length);

        // 数组算法：快排 + 二分
        int[] arr = new int[20000];
        long seed = 12345;
        for (int i = 0; i < arr.length; i++) { seed = seed * 6364136223846793005L + 1442695040888963407L; arr[i] = (int)((seed >>> 33) % 100000); }
        Arrays.sort(arr);
        int found = Arrays.binarySearch(arr, arr[7777]);
        long sum = 0; for (int v : arr) sum += v;
        System.out.println("sorted=" + arr[0] + ".." + arr[arr.length-1] + " found=" + found + " sum=" + sum);

        // 递归 + 记忆化
        System.out.println("fib25=" + fib(25) + " collatz=" + collatz(27));
        System.out.println("done");
    }
    static long fib(int n) { return n < 2 ? n : fib(n-1) + fib(n-2); }
    static int collatz(long n) { int c = 0; while (n != 1) { n = (n % 2 == 0) ? n/2 : 3*n+1; c++; } return c; }
}
