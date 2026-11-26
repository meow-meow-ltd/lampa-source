//! TOTP verification compatible with the legacy Node backend (`speakeasy`).
//!
//! Fixed profile (plan §5.5, T10): **SHA-1**, **6 digits**, **30 s** step,
//! **window = 1** (± one step). The secret is the base32 `TOTP_SECRET` value.
//!
//! Compatibility evidence: the test vectors below were produced by the pinned
//! `speakeasy` package (`node -e "require('speakeasy').totp({...})"`) for the
//! public S8 lab secret and by the RFC 6238 SHA-1 test seed. `verify` compares
//! every candidate window with [`crate::auth::constant_time::eq`] and never
//! returns early inside the window loop, so the comparison is timing-safe.

use std::fmt;

use totp_rs::{Algorithm, Secret, TOTP};

use crate::auth::constant_time;

/// TOTP hash algorithm (speakeasy default, RFC 6238 reference).
pub const ALGORITHM: Algorithm = Algorithm::SHA1;
/// Number of digits in a code.
pub const DIGITS: usize = 6;
/// TOTP time step in seconds.
pub const STEP_SECONDS: u64 = 30;
/// Accepted clock skew in steps (`window = 1`, i.e. -30 s / +30 s).
pub const WINDOW: u8 = 1;
/// Invalid `TOTP_SECRET` configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TotpConfigError {
    /// `TOTP_SECRET` is unset or empty (Node answers 500 "Server configuration error").
    MissingSecret,
    /// The value is not valid base32.
    InvalidBase32,
    /// The decoded secret is shorter than 128 bits.
    InvalidSecret,
}

impl fmt::Display for TotpConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TotpConfigError::MissingSecret => formatter.write_str("TOTP secret is not configured"),
            TotpConfigError::InvalidBase32 => {
                formatter.write_str("TOTP secret is not valid base32")
            }
            TotpConfigError::InvalidSecret => formatter.write_str("TOTP secret is too short"),
        }
    }
}

impl std::error::Error for TotpConfigError {}

/// Verifier for one configured TOTP secret.
#[derive(Clone)]
pub struct TotpVerifier {
    totp: TOTP,
}

impl fmt::Debug for TotpVerifier {
    // Never print the shared secret (or anything derived from it).
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TotpVerifier")
            .field("algorithm", &"SHA1")
            .field("digits", &DIGITS)
            .field("step_seconds", &STEP_SECONDS)
            .field("window", &WINDOW)
            .field("secret", &"***")
            .finish()
    }
}

impl TotpVerifier {
    /// Builds a verifier from the base32 `TOTP_SECRET` value.
    ///
    /// Lowercase and padded (`=`) base32 is accepted, matching what operators
    /// commonly paste into `.env`; decoding is RFC 4648 without padding.
    pub fn from_base32(secret: &str) -> Result<Self, TotpConfigError> {
        let normalized = secret.trim().trim_end_matches('=').to_ascii_uppercase();
        if normalized.is_empty() {
            return Err(TotpConfigError::MissingSecret);
        }
        let bytes = Secret::Encoded(normalized)
            .to_bytes()
            .map_err(|_| TotpConfigError::InvalidBase32)?;
        Self::from_bytes(bytes)
    }

    /// Builds a verifier from raw secret bytes (tests: RFC 6238 ASCII seed).
    pub fn from_bytes(secret: Vec<u8>) -> Result<Self, TotpConfigError> {
        if secret.is_empty() {
            return Err(TotpConfigError::InvalidSecret);
        }
        // `new_unchecked`: speakeasy (the legacy verifier) accepts any base32
        // secret length, including the 80-bit `JBSWY3DPEHPK3PXP` S8 lab secret;
        // the RFC's 128-bit minimum is thus not enforced for parity. The digits
        // and step are fixed by this profile, not by the secret.
        Ok(Self {
            totp: TOTP::new_unchecked(ALGORITHM, DIGITS, WINDOW, STEP_SECONDS, secret),
        })
    }

    /// Verifies `token` at `unix_seconds` with the ±1-step window.
    ///
    /// Every candidate code is compared, so the loop has no early exit on a
    /// matching window; the length/digit checks only depend on the presented
    /// (untrusted) token, never on the secret.
    pub fn verify(&self, token: &str, unix_seconds: u64) -> bool {
        if token.len() != DIGITS || !token.bytes().all(|byte| byte.is_ascii_digit()) {
            return false;
        }
        let step = unix_seconds / STEP_SECONDS;
        let first = step.saturating_sub(u64::from(WINDOW));
        let mut matched = false;
        for offset in 0..(2 * u64::from(WINDOW) + 1) {
            let expected = self
                .totp
                .generate((first + offset).saturating_mul(STEP_SECONDS));
            matched |= constant_time::eq(expected.as_bytes(), token.as_bytes());
        }
        matched
    }

    /// Deterministic code generation for tests (fixed clock, no system time).
    #[cfg(test)]
    pub fn generate_at(&self, unix_seconds: u64) -> String {
        self.totp.generate(unix_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Public S8 lab secret (`review/inv/S8-auth.md` §2) — never a production value.
    const S8_SECRET_BASE32: &str = "JBSWY3DPEHPK3PXP";
    /// RFC 6238 Appendix B SHA-1 seed as ASCII.
    const RFC_SEED_ASCII: &[u8] = b"12345678901234567890";

    /// `(unix_seconds, speakeasy SHA-1 6-digit code)` for [`S8_SECRET_BASE32`].
    const S8_VECTORS: &[(u64, &str)] = &[
        (59, "996554"),
        (1_111_111_109, "071271"),
        (1_111_111_111, "358462"),
        (1_234_567_890, "742275"),
        (2_000_000_000, "890699"),
        (20_000_000_000, "752434"),
    ];

    /// `(unix_seconds, RFC 6238 SHA-1 8-digit code truncated to 6)`.
    const RFC_VECTORS: &[(u64, &str)] = &[
        (59, "287082"),
        (1_111_111_109, "081804"),
        (1_111_111_111, "050471"),
        (1_234_567_890, "005924"),
        (2_000_000_000, "279037"),
        (20_000_000_000, "353130"),
    ];

    fn s8_verifier() -> TotpVerifier {
        TotpVerifier::from_base32(S8_SECRET_BASE32).expect("S8 secret is valid base32")
    }

    fn rfc_verifier() -> TotpVerifier {
        TotpVerifier::from_bytes(RFC_SEED_ASCII.to_vec()).expect("RFC seed is 160 bits")
    }

    #[test]
    fn s8_speakeasy_vectors_generate_and_verify() {
        let verifier = s8_verifier();
        let mut verified = 0;
        for (time, expected) in S8_VECTORS {
            assert_eq!(
                verifier.generate_at(*time),
                *expected,
                "generated code differs from speakeasy at t={time}"
            );
            assert!(
                verifier.verify(expected, *time),
                "S8 vector t={time} code={expected} must verify"
            );
            verified += 1;
        }
        assert_eq!(verified, S8_VECTORS.len(), "all S8 vectors executed");
    }

    #[test]
    fn rfc6238_sha1_vectors_verify_with_six_digits() {
        let verifier = rfc_verifier();
        let mut verified = 0;
        for (time, expected) in RFC_VECTORS {
            assert_eq!(verifier.generate_at(*time), *expected, "t={time}");
            assert!(verifier.verify(expected, *time), "RFC vector t={time}");
            verified += 1;
        }
        assert_eq!(verified, RFC_VECTORS.len(), "all RFC 6238 vectors executed");
    }

    #[test]
    fn window_one_accepts_only_the_adjacent_steps() {
        let verifier = s8_verifier();
        let now = 1_111_111_109;
        // 071271 = current step, 965766 = previous step, 358462 = next step,
        // 490635 = two steps ahead (must be rejected); all speakeasy vectors
        // for the S8 base32 secret.
        assert!(verifier.verify("071271", now));
        assert!(
            verifier.verify("965766", now),
            "previous step is inside window=1"
        );
        assert!(
            verifier.verify("358462", now),
            "next step is inside window=1"
        );
        assert!(
            !verifier.verify("490635", now),
            "window=1 must not reach +2 steps"
        );
    }

    #[test]
    fn rejects_malformed_tokens_without_panicking() {
        let verifier = s8_verifier();
        let now = 1_111_111_109;
        for token in [
            "", "0", "08180", "0818044", "08180a", "abcdef", " 081804", "081804 ",
        ] {
            assert!(
                !verifier.verify(token, now),
                "token {token:?} must be rejected"
            );
        }
        // Valid digits from a neighbouring day do not verify.
        assert!(!verifier.verify("000000", now));
        // u64::MAX does not overflow the step arithmetic.
        assert!(!verifier.verify("081804", u64::MAX));
    }

    #[test]
    fn lowercase_and_padded_base32_secrets_are_accepted() {
        let lower = TotpVerifier::from_base32("jbswy3dpehpk3pxp").expect("lowercase base32");
        assert_eq!(lower.generate_at(1_111_111_109), "071271");
        let padded = TotpVerifier::from_base32("JBSWY3DPEHPK3PXP====").expect("padded base32");
        assert_eq!(padded.generate_at(1_111_111_109), "071271");
        assert_eq!(
            TotpVerifier::from_base32("   ").expect_err("blank secret"),
            TotpConfigError::MissingSecret
        );
        assert_eq!(
            TotpVerifier::from_base32("not-base32!").expect_err("invalid base32"),
            TotpConfigError::InvalidBase32
        );
        // The 80-bit S8 lab secret is deliberately accepted (speakeasy parity);
        // only an empty secret is a configuration error.
        assert!(TotpVerifier::from_base32("JBSWY3DPEHPK3P").is_ok());
        assert_eq!(
            TotpVerifier::from_bytes(Vec::new()).expect_err("empty secret"),
            TotpConfigError::InvalidSecret
        );
    }

    #[test]
    fn debug_output_redacts_the_secret() {
        let verifier = s8_verifier();
        let debug = format!("{verifier:?}");
        assert!(debug.contains("***"));
        assert!(!debug.contains(S8_SECRET_BASE32));
        assert!(!debug.contains("JBSWY3"));
        assert!(debug.contains("SHA1"));
        assert!(debug.contains("window: 1"));
    }
}
