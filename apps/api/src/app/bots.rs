//! Bot definition rules.

pub const MAX_NAME_CHARS: usize = 80;

/// Name of the main bot created once by explicit setup.
pub const MAIN_BOT_NAME: &str = "Assistant";

const WELCOME: &str = "Hi, I'm {bot_name}. Welcome to else.bot.

I'm your main assistant. Tell me what you need in your own words, whether it's a question, \
something you're working on, or a task you want me to handle.

I remember our conversations, so we can build on what you've already told me about your work, \
your life, and how you like things done. You can correct me whenever I get something wrong.

I have my own computer for browsing, writing code, and working with files. You can open its \
desktop to see what I'm doing. You can also create other bots for specific roles, and I can hand \
tasks to them when it helps.

To get acquainted, what should I call you? Tell me a little about yourself and what you'd like \
my help with first. We can figure out the rest as we go.";

/// The main bot's first message on a fresh installation.
pub fn welcome_message(name: &BotName) -> String {
    WELCOME.replace("{bot_name}", name.as_str())
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidBot {
    #[error("Name must be 1 to 80 characters after trimming.")]
    Name,
    #[error("Name must not contain NUL characters.")]
    Nul,
}

/// Trimmed display name. Duplicates are allowed; IDs identify bots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotName(String);

impl BotName {
    pub fn parse(raw: &str) -> Result<Self, InvalidBot> {
        if raw.contains('\0') {
            return Err(InvalidBot::Nul);
        }
        let name = raw.trim();
        let chars = name.chars().count();
        if chars == 0 || chars > MAX_NAME_CHARS {
            return Err(InvalidBot::Name);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules() {
        assert_eq!(BotName::parse("  Work ").unwrap().as_str(), "Work");
        assert_eq!(BotName::parse("   "), Err(InvalidBot::Name));
        assert!(BotName::parse(&"é".repeat(80)).is_ok());
        assert_eq!(BotName::parse(&"é".repeat(81)), Err(InvalidBot::Name));
        assert_eq!(BotName::parse("a\0b"), Err(InvalidBot::Nul));
        assert!(BotName::parse(MAIN_BOT_NAME).is_ok());
    }
}
