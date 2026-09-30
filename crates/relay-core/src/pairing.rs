//! Short pairing codes (`NN-NNNN-NNNN`).
//!
//! The first two digits are a public nameplate used only to pick the right
//! device during discovery. The remaining eight digits are the secret.

use rand::Rng;

use crate::error::CoreError;

/// Ten decimal digits that identify one pairing attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PairingCode {
    digits: [u8; 10],
}

impl PairingCode {
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let mut digits = [0u8; 10];
        for slot in &mut digits {
            *slot = b'0' + rng.gen_range(0..10);
        }
        Self { digits }
    }

    /// Accepts `NN-NNNN-NNNN`, the same digits with spaces, or 10 bare digits.
    pub fn parse(input: &str) -> Result<Self, CoreError> {
        let mut digits = [0u8; 10];
        let mut n = 0usize;
        for ch in input.chars() {
            if ch == '-' || ch.is_ascii_whitespace() {
                continue;
            }
            if !ch.is_ascii_digit() {
                return Err(CoreError::InvalidPairingCode {
                    value: input.to_owned(),
                    reason: "only digits, spaces and dashes are allowed",
                });
            }
            if n >= 10 {
                return Err(CoreError::InvalidPairingCode {
                    value: input.to_owned(),
                    reason: "expected 10 digits",
                });
            }
            digits[n] = ch as u8;
            n += 1;
        }
        if n != 10 {
            return Err(CoreError::InvalidPairingCode {
                value: input.to_owned(),
                reason: "expected 10 digits",
            });
        }
        Ok(Self { digits })
    }

    pub fn format(&self) -> String {
        let d = std::str::from_utf8(&self.digits).expect("ascii digits");
        format!("{}-{}-{}", &d[..2], &d[2..6], &d[6..])
    }

    pub fn digits(&self) -> &str {
        std::str::from_utf8(&self.digits).expect("ascii digits")
    }

    pub fn nameplate(&self) -> &str {
        &self.digits()[..2]
    }

    pub fn password_bytes(&self) -> &[u8] {
        &self.digits
    }
}

impl std::fmt::Display for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.format())
    }
}

impl std::str::FromStr for PairingCode {
    type Err = CoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_nn_nnnn_nnnn() {
        let code = PairingCode::parse("1234567890").unwrap();
        assert_eq!(code.format(), "12-3456-7890");
        assert_eq!(code.nameplate(), "12");
        assert_eq!(code.digits(), "1234567890");
        assert_eq!(code.password_bytes(), b"1234567890");
    }

    #[test]
    fn parse_accepts_dashes_and_spaces() {
        for raw in ["12-3456-7890", "12 3456 7890", "1234567890", "12-3456 7890"] {
            assert_eq!(
                PairingCode::parse(raw).unwrap().digits(),
                "1234567890",
                "{raw}"
            );
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        for raw in ["", "123", "12345678901", "12-3456-789a", "12-3456-789"] {
            assert!(PairingCode::parse(raw).is_err(), "{raw} accepted");
        }
    }

    #[test]
    fn generated_codes_round_trip() {
        for _ in 0..32 {
            let code = PairingCode::generate();
            assert_eq!(code.nameplate().len(), 2);
            assert_eq!(PairingCode::parse(&code.format()).unwrap(), code);
            assert_eq!(PairingCode::parse(code.digits()).unwrap(), code);
        }
    }

    #[test]
    fn nameplate_matches_same_prefix() {
        let a = PairingCode::parse("12-3456-7890").unwrap();
        let b = PairingCode::parse("12-0000-0000").unwrap();
        let c = PairingCode::parse("13-3456-7890").unwrap();
        assert_eq!(a.nameplate(), b.nameplate());
        assert_ne!(a.nameplate(), c.nameplate());
    }
}
