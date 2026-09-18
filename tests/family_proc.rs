//! Ground-truth tests for the `proc` family.
//!
//! Owned by that family's thread; see BRIEF.md. A test here checks a
//! measurement against something independently known -- a kernel counter, an
//! injected delay, a known size, a fixed iteration count. A test that checks
//! the code returns what the code computed proves nothing, which is the whole
//! difficulty of testing a tracer.

#[test]
fn placeholder_until_this_family_has_a_collector() {}
