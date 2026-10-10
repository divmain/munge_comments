/* Rust block comments nest: /* inner */ still inside the outer one. */
pub fn nested() {}

pub fn value() -> i32 {
    /* one /* two /* three */ two */ one */
    42
}
