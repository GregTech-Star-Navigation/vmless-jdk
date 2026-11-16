// do-while：至少执行一次；`continue` 要跳到条件判断处
class DoWhile1 {
    public static void main(String[] args) {
        int i = 0;
        do {
            System.out.println(i);
            i = i + 1;
        } while (i < 3);

        // continue 跳到条件判断，而不是循环头
        int n = 0;
        do {
            n = n + 1;
            if (n == 2) {
                continue;
            }
            System.out.println("n=" + n);
        } while (n < 4);

        // break 直接跳出
        int k = 10;
        do {
            k = k + 1;
            if (k > 12) {
                break;
            }
            System.out.println("k=" + k);
        } while (true);

        // 条件一开始就是假：体仍然跑一次
        int c = 0;
        do {
            System.out.println("once " + c);
            c = c + 1;
        } while (false);

        // 带标签的 continue 跳出外层 do-while
        int total = 0;
        int a = 0;
        outer:
        do {
            a = a + 1;
            int b = 0;
            do {
                b = b + 1;
                if (b == 2) {
                    continue outer;
                }
                total = total + a * b;
            } while (b < 3);
        } while (a < 3);
        System.out.println(total);
    }
}
