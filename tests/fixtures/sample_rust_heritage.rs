// Fixture for Rust heritage extraction tests.

pub trait Display {
    fn show(&self);
}

pub trait Pretty: Display {
    fn pretty(&self);
}

pub struct Widget;

impl Display for Widget {
    fn show(&self) {}
}
