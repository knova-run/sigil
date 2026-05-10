package com.example.app

import kotlin.collections.List
import kotlin.io.println as p

const val MAX_RETRIES: Int = 3

val GREETING: String = "Hello, world"

class Person(val name: String, var age: Int) {
    private val createdAt: Long = System.currentTimeMillis()

    fun greet(): String {
        return "Hi, $name"
    }

    private fun helper() {}

    companion object {
        const val SPECIES = "human"
        fun create(name: String): Person = Person(name, 0)
    }
}

object Singleton {
    val id: Int = 1
    fun work() {}
}

interface Greeter {
    fun greet(): String
}

fun standalone(x: Int, y: Int): Int {
    return x + y
}

internal fun internalHelper() {}
