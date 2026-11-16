class Label1 {
    public static void main(String[] args) {
        // 标签 break：直接跳出两层
        outer:
        for (int i = 0; i < 4; i = i + 1) {
            for (int j = 0; j < 4; j = j + 1) {
                if (i * 4 + j == 6) {
                    break outer;
                }
                System.out.println(i * 10 + j);
            }
        }
        System.out.println(-1);

        // 标签 continue：只结束外层这一轮
        row:
        for (int i = 0; i < 3; i = i + 1) {
            for (int j = 0; j < 3; j = j + 1) {
                if (j == 1) {
                    continue row;
                }
                System.out.println(i * 10 + j);
            }
        }
        System.out.println(-2);
    }
}
