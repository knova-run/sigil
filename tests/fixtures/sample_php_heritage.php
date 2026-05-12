<?php

class Animal {
    public function breathe() {}
}

interface Runnable {
    public function run(): void;
}

class Dog extends Animal implements Runnable {
    public function run(): void {}
}
