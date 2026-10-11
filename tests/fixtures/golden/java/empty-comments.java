public class Counter {
    private int count;

    public void increment() {
        //
        count++;
        /* */
        if (count < 0) {
            count = 0; //
        }
        // ------
    }
}
