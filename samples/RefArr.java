class RefArr {
    public static void main(String[] args) {
        String[] arr = new String[3];
        arr[0] = "a";
        arr[1] = "bb";
        arr[2] = "ccc";
        System.out.println(arr[0]);
        System.out.println(arr[1]);
        System.out.println(arr[2]);
        System.out.println(arr.length);
        int i = 0;
        while (i < arr.length) {
            System.out.println(arr[i]);
            i = i + 1;
        }
        String s = arr[1];
        System.out.println(s);
        arr[1] = arr[2];
        System.out.println(arr[1]);
    }
}
