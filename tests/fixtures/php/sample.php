<?php

namespace App\Service;

use App\Util\Logger;
use App\Util\Cache as C;

const MAX_RETRIES = 3;

class Person {
    public string $name;
    private int $age;

    const SPECIES = "human";

    public function __construct(string $name, int $age) {
        $this->name = $name;
        $this->age = $age;
    }

    public function greet(): string {
        return "Hi, " . $this->name;
    }

    private function helper(): void {
        // intentionally empty
    }
}

interface Greeter {
    public function greet(): string;
}

trait Helpful {
    public function help(): void {}
}

enum Status {
    case Active;
    case Inactive;
}

function standalone(int $x, int $y): int {
    return $x + $y;
}
