// Fixture for Java heritage extraction tests.
// Mirrors the shape of tests/fixtures/sample_heritage.go.

class Animal {
    void breathe() {}
}

interface Runnable {
    void run();
}

interface Swimmer {
    void swim();
}

class Dog extends Animal implements Runnable, Swimmer {
    public void run() {}
    public void swim() {}
}

interface Pet extends Runnable, Swimmer {
}
