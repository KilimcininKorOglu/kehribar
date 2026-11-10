//! Parsing of HTML form fields shared by the pages.

use crate::crypto::Cipher;
use crate::error::AppError;

/// A form field that is not a whole number in its allowed range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidNumber;

/// Parses an optional whole number in `range`; an empty field is `None`.
pub fn optional_number(
    text: Option<&str>,
    range: std::ops::RangeInclusive<i64>,
) -> Result<Option<i64>, InvalidNumber> {
    match optional_text(text) {
        None => Ok(None),
        Some(text) => text
            .parse()
            .ok()
            .filter(|value| range.contains(value))
            .map(Some)
            .ok_or(InvalidNumber),
    }
}

/// The trimmed field, or `None` when it is missing or blank.
pub fn optional_text(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// A checkbox sends a value only when it is checked.
pub fn is_checked(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty())
}

/// Encrypts a token field. A blank field gives `None`. Tokens are not
/// trimmed inside, but surrounding whitespace from a paste is removed.
pub fn encrypt_token(cipher: &Cipher, text: Option<&str>) -> Result<Option<Vec<u8>>, AppError> {
    optional_text(text)
        .map(|token| cipher.encrypt(token).map_err(AppError::internal))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_number_accepts_blank_and_values_in_range() {
        assert_eq!(optional_number(None, 1..=10), Ok(None));
        assert_eq!(optional_number(Some("  "), 1..=10), Ok(None));
        assert_eq!(optional_number(Some(" 7 "), 1..=10), Ok(Some(7)));
        for bad in ["0", "11", "5m"] {
            assert_eq!(
                optional_number(Some(bad), 1..=10),
                Err(InvalidNumber),
                "{bad}"
            );
        }
    }

    #[test]
    fn blank_token_is_none_and_a_token_round_trips() {
        let cipher = Cipher::new(&[7; 32]);
        assert_eq!(encrypt_token(&cipher, Some("   ")).unwrap(), None);
        assert_eq!(encrypt_token(&cipher, None).unwrap(), None);
        let stored = encrypt_token(&cipher, Some(" ghp_abc \n"))
            .unwrap()
            .unwrap();
        assert_eq!(cipher.decrypt(&stored).unwrap(), "ghp_abc");
    }
}
