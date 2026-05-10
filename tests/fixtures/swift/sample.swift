import Foundation
import UIKit

let MAX_RETRIES: Int = 3
var globalCounter = 0

struct Point {
    let x: Double
    var y: Double

    func magnitude() -> Double {
        return (x * x + y * y).squareRoot()
    }
}

class Person {
    public let name: String
    private var age: Int

    init(name: String, age: Int) {
        self.name = name
        self.age = age
    }

    public func greet() -> String {
        return "Hi, \(name)"
    }

    private func helper() {
        print(name)
    }
}

protocol Greeter {
    func greet() -> String
    var greeting: String { get }
}

enum Direction {
    case north
    case south
    case east
    case west
}

extension Person {
    func describe() -> String {
        return "Person named \(name)"
    }
}

func standalone(x: Int, y: Int) -> Int {
    return x + y
}

internal func internalHelper() {}

fileprivate func filePrivateHelper() {}
