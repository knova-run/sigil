// Fixture for TypeScript heritage extraction tests.

class Animal {}

interface Runnable {
    run(): void;
}

interface Swimmer {
    swim(): void;
}

export class Dog extends Animal implements Runnable, Swimmer {
    run() {}
    swim() {}
}

export interface Pet extends Runnable, Swimmer {}
