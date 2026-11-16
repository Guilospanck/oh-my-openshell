//! One answer to "are we running under a test?" for code that lives in this crate.
//!
//! `cfg!(test)` is true only while the crate whose own tests run is compiled, so a guard written
//! with it silently turns off when the osh-tui tests link osh-core. osh-tui enables the `testing`
//! feature as a dev-dependency only, so release and dev builds of the binary leave it off.

/// True in osh-core's own tests and in any build that enables the `testing` feature (osh-tui's
/// tests). Use it, not `cfg!(test)`, for guards that keep tests off real state and live tools.
pub const fn under_test() -> bool {
    cfg!(any(test, feature = "testing"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_is_on_in_this_crates_own_tests() {
        assert!(super::under_test());
    }
}
