package com.example.app

import scala.collection.immutable.List
import scala.util.{Try, Success}

val MAX_RETRIES: Int = 3

val greeting: String = "Hello, world"

class Person(val name: String, var age: Int) {
  private val createdAt: Long = System.currentTimeMillis()

  def greet(): String = "Hi, " + name

  private def helper(): Unit = ()
}

object Singleton {
  val id: Int = 1
  def work(): Unit = ()
}

trait Greeter {
  def greet(): String
}

def standalone(x: Int, y: Int): Int = x + y

sealed class Shape
