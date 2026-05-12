open class Animal {
    fun breathe() {}
}

interface Runnable {
    fun run()
}

class Dog : Animal(), Runnable {
    override fun run() {}
}
