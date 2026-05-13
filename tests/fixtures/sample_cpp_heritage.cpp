class Animal {
public:
    void breathe() {}
};

class Runnable {
public:
    virtual void run() = 0;
};

class Dog : public Animal, public Runnable {
public:
    void run() override {}
};
