//! Fixture library crate root.

pub mod util;

pub fn answer() -> u32 {
    util::math::add(20, 22)
}
