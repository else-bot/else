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

/// A preset avatar: one of the shapes the desktop draws, in one of its
/// named colors. The API stores and validates the names; it draws nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Avatar {
    pub shape: AvatarShape,
    pub color: AvatarColor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum AvatarShape {
    Star,
    Round,
    Cloud,
    Tile,
    Sparkle,
    Flower,
    Egg,
    Bean,
    Cone,
    House,
    Gem,
    Ghost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum AvatarColor {
    White,
    Brown,
    Red,
    Orange,
    Amber,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
    Pink,
    Gray,
}

impl Avatar {
    /// The main bot's avatar, and what the desktop's Reset restores.
    pub const DEFAULT: Self = Self {
        shape: AvatarShape::Star,
        color: AvatarColor::Blue,
    };

    /// A stable starting avatar for a new bot, so bots differ at a glance
    /// before the owner picks one.
    pub fn derived(bot_id: uuid::Uuid) -> Self {
        use AvatarColor::*;
        use AvatarShape::*;
        const SHAPES: [AvatarShape; 12] = [
            Star, Round, Cloud, Tile, Sparkle, Flower, Egg, Bean, Cone, House, Gem, Ghost,
        ];
        const COLORS: [AvatarColor; 8] = [Blue, Teal, Green, Pink, Purple, Orange, Red, Amber];
        let bytes = bot_id.as_bytes();
        Self {
            shape: SHAPES[usize::from(bytes[0]) % SHAPES.len()],
            color: COLORS[usize::from(bytes[1]) % COLORS.len()],
        }
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

    #[test]
    fn avatars_use_names_on_the_wire() {
        assert_eq!(
            serde_json::to_value(Avatar::DEFAULT).unwrap(),
            serde_json::json!({ "shape": "star", "color": "blue" })
        );
        for bad in [
            serde_json::json!({ "shape": "dragon", "color": "blue" }),
            serde_json::json!({ "shape": "star", "color": "#0000ff" }),
            serde_json::json!({ "shape": "star" }),
            serde_json::json!({ "shape": "star", "color": "blue", "image": "x" }),
        ] {
            assert!(
                serde_json::from_value::<Avatar>(bad.clone()).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn derived_avatars_are_stable_per_bot() {
        let id = uuid::Uuid::from_bytes([3, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(Avatar::derived(id), Avatar::derived(id));
        assert_eq!(
            Avatar::derived(id),
            Avatar {
                shape: AvatarShape::Tile,
                color: AvatarColor::Teal
            }
        );
    }
}
