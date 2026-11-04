use gpui_kit::component::input::{InlineToken, InputContent};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[cfg(target_os = "linux")]
pub mod clipboard;

pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
pub const MAX_FILES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Uuid,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

impl Attachment {
    pub fn fallback(&self) -> String {
        format!("[File: {}]", self.name)
    }

    pub fn token(&self) -> InlineToken {
        InlineToken::new(self.id.to_string(), self.fallback()).with_label(self.name.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    File { file: Attachment },
}

pub fn input_content(text: &str, parts: &[MessagePart]) -> InputContent {
    let mut content = InputContent::new(text.to_owned());
    let mut offset = 0;
    for part in parts {
        match part {
            MessagePart::Text { text } => offset += text.len(),
            MessagePart::File { file } => {
                let end = offset + file.fallback().len();
                let Ok(next) = content.with_token(offset..end, file.token()) else {
                    return InputContent::new(text.to_owned());
                };
                content = next;
                offset = end;
            }
        }
    }
    content
}

pub fn file_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_stay_between_their_surrounding_unicode_text() {
        let file = Attachment {
            id: Uuid::new_v4(),
            name: "écran.png".into(),
            media_type: "image/png".into(),
            size: 42,
        };
        let parts = vec![
            MessagePart::Text {
                text: "Avant 🥔 ".into(),
            },
            MessagePart::File { file: file.clone() },
            MessagePart::Text {
                text: " après".into(),
            },
        ];
        let text = format!("Avant 🥔 {} après", file.fallback());
        let content = input_content(&text, &parts);
        assert_eq!(content.text().as_ref(), text);
        assert_eq!(content.tokens().len(), 1);
        assert_eq!(&text[content.tokens()[0].range()], file.fallback());
        assert_eq!(content.tokens()[0].token().label().as_ref(), "écran.png");
    }

    #[test]
    fn an_inconsistent_server_payload_remains_readable_without_tokens() {
        let file = Attachment {
            id: Uuid::new_v4(),
            name: "image.png".into(),
            media_type: "image/png".into(),
            size: 42,
        };
        let content = input_content("older server text", &[MessagePart::File { file }]);
        assert!(content.tokens().is_empty());
        assert_eq!(content.text().as_ref(), "older server text");
    }
}
