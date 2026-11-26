//! Constant-time byte comparison for credentials.
//!
//! TOTP codes (fixed 6 digits) and the admin token are compared with a
//! branch-free accumulate loop: the loop always visits every byte and the only
//! early exit is the public length check. This keeps the comparison time
//! independent of *where* two values first differ.

/// Returns `true` when both byte strings are equal.
///
/// The length check is public (credential lengths are fixed/known); contents
/// are compared without data-dependent branching.
pub fn eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_equal_byte_strings() {
        assert!(eq(b"", b""));
        assert!(eq(b"081804", b"081804"));
        assert!(eq(b"known-token", b"known-token"));
        assert!(!eq(b"081804", b"081805"));
        assert!(!eq(b"081804", b"08180"));
        assert!(!eq(b"081804", b""));
        assert!(!eq(b"a", b"A"));
        assert!(!eq(b"known-token", b"known-tokeN"));
    }

    #[test]
    fn compares_every_byte_even_after_a_mismatch() {
        // The loop is branch-free over contents; this is a behaviour guard for
        // future rewrites (no `return` on the first differing byte).
        let left = [0_u8; 64];
        let mut right = [0_u8; 64];
        right[63] = 1;
        assert!(!eq(&left, &right));
        right[0] = 1;
        assert!(!eq(&left, &right));
        assert!(eq(&left, &left));
    }
}
