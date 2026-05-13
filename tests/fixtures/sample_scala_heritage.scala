class Animal {
  def breathe(): Unit = {}
}

trait Runnable {
  def run(): Unit
}

class Dog extends Animal with Runnable {
  override def run(): Unit = {}
}
