//! A macro for enums that are stored, sent and parsed as fixed text codes.

/// Declares a fieldless enum with one text code per variant and generates
/// `ALL`, `code()` and `parse()` for it.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$variant_meta:meta])* $variant:ident => $code:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $( $(#[$variant_meta])* $variant ),+
        }

        impl $name {
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] = [$(Self::$variant),+];

            pub fn code(self) -> &'static str {
                match self {
                    $( Self::$variant => $code ),+
                }
            }

            pub fn parse(value: &str) -> Option<Self> {
                Self::ALL.into_iter().find(|item| item.code() == value)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    text_enum! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        enum Color {
            Red => "red",
            /// Documented variant.
            DarkBlue => "dark-blue",
        }
    }

    #[test]
    fn codes_round_trip_and_unknown_text_is_rejected() {
        assert_eq!(Color::ALL, [Color::Red, Color::DarkBlue]);
        for color in Color::ALL {
            assert_eq!(Color::parse(color.code()), Some(color));
        }
        assert_eq!(Color::DarkBlue.code(), "dark-blue");
        assert_eq!(Color::parse("Red"), None);
        assert_eq!(Color::parse(""), None);
    }
}
