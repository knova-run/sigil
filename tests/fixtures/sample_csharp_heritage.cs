class Animal { public void Breathe() {} }

interface IRunnable { void Run(); }

class Dog : Animal, IRunnable {
    public void Run() {}
}
