class Animal {}

protocol Runnable {
    func run()
}

class Dog: Animal, Runnable {
    func run() {}
}
