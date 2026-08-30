fn alpha() -> i32 {
    let beta = 42;
    beta + 1
}

struct Widget {
    name: String,
}

impl Widget {
    fn new(name: &str) -> Self {
        Widget { name: name.to_string() }
    }
}
