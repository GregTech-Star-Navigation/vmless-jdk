class Multi {
    public static void main(String[] args) {
        int[][] m = new int[3][4];
        for (int i = 0; i < 3; i = i + 1) {
            for (int j = 0; j < 4; j = j + 1) {
                m[i][j] = i * 10 + j;
            }
        }
        int sum = 0;
        for (int i = 0; i < 3; i = i + 1) {
            for (int j = 0; j < 4; j = j + 1) {
                sum = sum + m[i][j];
            }
        }
        System.out.println(sum);
        System.out.println(m.length);
        System.out.println(m[0].length);
        System.out.println(m[2][3]);

        String[][] s = new String[2][3];
        for (int i = 0; i < 2; i = i + 1) {
            for (int j = 0; j < 3; j = j + 1) {
                s[i][j] = "x";
            }
        }
        System.out.println(s[1][2]);
        System.out.println(s[0][0]);
        System.out.println(s.length);

        int[][] jag = new int[3][];
        for (int i = 0; i < 3; i = i + 1) {
            jag[i] = new int[i + 1];
            for (int j = 0; j <= i; j = j + 1) {
                jag[i][j] = i + j;
            }
        }
        int t = 0;
        for (int i = 0; i < 3; i = i + 1) {
            t = t + jag[i][i];
        }
        System.out.println(t);
        System.out.println(jag[2].length);
        System.out.println(jag[0].length);
    }
}
