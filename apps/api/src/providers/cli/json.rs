//! Field readers for a CLI's JSON events. A field of the wrong type makes
//! the event malformed, which is a policy violation.

use serde_json::Value;

use super::attempt::Violation;

/// The readers for one provider, whose label names it in violations.
#[derive(Debug, Clone, Copy)]
pub struct Fields(pub &'static str);

impl Fields {
    pub fn malformed(self, what: &str) -> Violation {
        Violation::policy(format!("{} sent a malformed {what}", self.0))
    }

    /// A string, or nothing.
    pub fn text<'v>(self, value: &'v Value, what: &str) -> Result<Option<&'v str>, Violation> {
        match value {
            Value::Null => Ok(None),
            Value::String(text) => Ok(Some(text)),
            _ => Err(self.malformed(what)),
        }
    }

    pub fn required<'v>(self, value: &'v Value, what: &str) -> Result<&'v str, Violation> {
        self.text(value, what)?.ok_or_else(|| self.malformed(what))
    }

    /// An array that must be present.
    pub fn list<'v>(self, value: &'v Value, what: &str) -> Result<&'v [Value], Violation> {
        value
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| self.malformed(what))
    }

    /// An array, with nothing read as an empty one.
    pub fn list_or_empty<'v>(self, value: &'v Value, what: &str) -> Result<&'v [Value], Violation> {
        match value {
            Value::Null => Ok(&[]),
            _ => self.list(value, what),
        }
    }
}
