//! The suites, and what they cover.

pub mod parse;
pub mod store;

/// Every suite that runs headlessly, in the order a full sweep runs them.
///
/// `store` first because it builds the corpus the others borrow, and because
/// it is the slow one — a sweep that is going to be interrupted should have
/// produced the expensive numbers before it was.
pub const HEADLESS: [&str; 2] = ["store", "parse"];

/// Every benchmark id a full headless sweep can produce.
pub fn ids() -> Vec<String> {
    let mut ids = store::ids();
    ids.extend(parse::ids());
    ids
}
