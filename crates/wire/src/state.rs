/// Declares a state sent as a string. Any other string decodes to the
/// fallback variant, so a newer server's state still loads. Unlike
/// `#[serde(other)]`, a value that is not a string fails to decode, so a
/// malformed event still resets the stream.
///
/// A unit fallback (`_ => Unknown`) gives a `Copy` enum and exists only on
/// the reading side: the server never builds it. A fallback that keeps the
/// text (`_ => Other(String)`) serializes back to that text, for states a
/// panel prints or the server sends as free text.
macro_rules! wire_state {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $text:literal,)*
        }
        $(#[$fallback_meta:meta])*
        _ => $fallback:ident(String)
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
        #[serde(from = "String")]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
            $(#[$fallback_meta])*
            $fallback(String),
        }

        impl From<String> for $name {
            fn from(text: String) -> Self {
                match text.as_str() {
                    $($text => Self::$variant,)*
                    _ => Self::$fallback(text),
                }
            }
        }

        impl $name {
            pub fn as_str(&self) -> &str {
                match self {
                    $(Self::$variant => $text,)*
                    Self::$fallback(text) => text,
                }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $text:literal,)*
        }
        $(#[$fallback_meta:meta])*
        _ => $fallback:ident
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
        #[serde(from = "String")]
        pub enum $name {
            $($(#[$variant_meta])* $variant,)*
            $(#[$fallback_meta])*
            $fallback,
        }

        impl From<String> for $name {
            fn from(text: String) -> Self {
                match text.as_str() {
                    $($text => Self::$variant,)*
                    _ => Self::$fallback,
                }
            }
        }

        impl $name {
            /// The wire text; the fallback, which only a reader builds,
            /// writes `unknown`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text,)*
                    Self::$fallback => "unknown",
                }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    wire_state! {
        pub enum Texted {
            One = "one",
        }
        _ => Other(String)
    }

    #[test]
    fn a_kept_text_writes_back_unchanged() {
        let written = serde_json::to_string(&[Texted::One, Texted::Other("two".into())]).unwrap();
        assert_eq!(written, r#"["one","two"]"#);
    }
}
