//! Geometry shapes — generics, a trait with a default method, and two more
//! intentional same-name-method collisions (`area`, `scale`) across two
//! distinct types (`Circle`, `Square`).

use std::fmt;

pub const PI: f64 = 3.14159265358979;

pub type Length = f64;

#[derive(Debug, Clone, Copy)]
pub struct Circle {
    pub radius: Length,
}

#[derive(Debug, Clone, Copy)]
pub struct Square {
    pub side: Length,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Orientation {
    Portrait,
    Landscape,
    Square,
}

pub trait Area {
    fn area(&self) -> Length;

    fn describe_area(&self) -> String {
        format!("{:.2}", self.area())
    }
}

impl Area for Circle {
    fn area(&self) -> Length {
        PI * self.radius * self.radius
    }
}

impl Area for Square {
    fn area(&self) -> Length {
        self.side * self.side
    }
}

impl Circle {
    pub fn new(radius: Length) -> Self {
        Circle { radius }
    }

    pub fn scale(&self, factor: Length) -> Self {
        Circle {
            radius: self.radius * factor,
        }
    }
}

impl Square {
    pub fn new(side: Length) -> Self {
        Square { side }
    }

    pub fn scale(&self, factor: Length) -> Self {
        Square {
            side: self.side * factor,
        }
    }
}

impl fmt::Display for Circle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "circle(r={})", self.radius)
    }
}

pub fn largest_area<T: Area>(items: &[T]) -> Option<Length> {
    items.iter().map(|i| i.area()).fold(None, |acc, a| {
        Some(acc.map_or(a, |m: Length| if a > m { a } else { m }))
    })
}

pub mod units {
    use super::Length;

    pub const CM_PER_INCH: Length = 2.54;

    pub fn to_cm(inches: Length) -> Length {
        inches * CM_PER_INCH
    }
}
