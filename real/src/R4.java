// R4：纯算法（排序/矩阵/字符串），只用 System.out —— 绕开静态调用缺口
public class R4 {
    static void qs(int[] a, int lo, int hi) {
        if (lo >= hi) return;
        int p = a[(lo + hi) >>> 1], i = lo, j = hi;
        while (i <= j) {
            while (a[i] < p) i++;
            while (a[j] > p) j--;
            if (i <= j) { int t = a[i]; a[i] = a[j]; a[j] = t; i++; j--; }
        }
        qs(a, lo, j); qs(a, i, hi);
    }
    static void msort(int[] a, int[] tmp, int lo, int hi) {
        if (hi - lo < 2) return;
        int mid = (lo + hi) >>> 1;
        msort(a, tmp, lo, mid); msort(a, tmp, mid, hi);
        int i = lo, j = mid, k = lo;
        while (i < mid && j < hi) tmp[k++] = (a[i] <= a[j]) ? a[i++] : a[j++];
        while (i < mid) tmp[k++] = a[i++];
        while (j < hi) tmp[k++] = a[j++];
        for (int t = lo; t < hi; t++) a[t] = tmp[t];
    }
    public static void main(String[] args) {
        int n = 30000;
        int[] a = new int[n], b = new int[n];
        long s = 88;
        for (int i = 0; i < n; i++) { s = s * 6364136223846793005L + 1442695040888963407L; a[i] = (int)((s >>> 33) % 1000000); }
        for (int i = 0; i < n; i++) b[i] = a[i];
        qs(a, 0, n - 1);
        msort(b, new int[n], 0, n);
        boolean same = true;
        long sum = 0;
        for (int i = 0; i < n; i++) { if (a[i] != b[i]) same = false; sum += a[i]; }
        System.out.println("same=" + same + " min=" + a[0] + " max=" + a[n-1] + " sum=" + sum);

        // 矩阵乘
        int m = 80;
        int[][] x = new int[m][m], y = new int[m][m], z = new int[m][m];
        for (int i = 0; i < m; i++) for (int j = 0; j < m; j++) { x[i][j] = (i * 7 + j * 13) % 100; y[i][j] = (i * 11 + j * 3) % 100; }
        for (int i = 0; i < m; i++) for (int j = 0; j < m; j++) { int t = 0; for (int k = 0; k < m; k++) t += x[i][k] * y[k][j]; z[i][j] = t; }
        System.out.println("mat=" + z[0][0] + "," + z[m-1][m-1] + "," + z[m/2][m/2]);

        // 字符串构建（StringBuilder 全是实例方法）
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < 3000; i++) sb.append((char)('a' + i % 26));
        String str = sb.toString();
        int cnt = 0; char pc = 0;
        for (int i = 0; i < str.length(); i++) { char c = str.charAt(i); if (c != pc) { cnt++; pc = c; } }
        System.out.println("len=" + str.length() + " runs=" + cnt + " first=" + str.charAt(0) + " last=" + str.charAt(str.length()-1));

        // 素数筛
        int lim = 200000;
        boolean[] comp = new boolean[lim + 1];
        int primes = 0;
        for (int i = 2; i <= lim; i++) { if (!comp[i]) { primes++; for (long j = (long)i * i; j <= lim; j += i) comp[(int)j] = true; } }
        System.out.println("primes=" + primes);
        System.out.println("done");
    }
}
