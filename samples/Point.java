class Point {
    int x;
    Point(int v) { this.x = v; }
    static int twice(Point p) { return p.x + p.x; }
    public static void main(String[] args) {
        Point p = new Point(21);
        System.out.println(twice(p));
    }
}
