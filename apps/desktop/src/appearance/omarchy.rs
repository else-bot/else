//! Omarchy's live palette: where it lives, how it parses, and the watch that
//! sees `omarchy-theme-set` swap it.
//!
//! The current theme is `~/.local/state/omarchy/current/theme/colors.toml`.
//! Older installs kept it under `~/.config/omarchy/current`; that location is
//! read only when the modern state directory does not exist at all, so a
//! leftover legacy theme never outranks a broken modern one.

use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::App;
use gpui_kit::component::ThemeMode;
use notify::Watcher as _;
use tokio::sync::mpsc;

use super::{Palette, Rgb};

/// `omarchy-theme-set` replaces the theme in several steps; reading once
/// they settle avoids parsing a half-moved directory.
const SETTLE: Duration = Duration::from_millis(60);

#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error("Omarchy is not installed")]
    NotInstalled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("colors.toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("colors.toml has no {0}")]
    Missing(&'static str),
    #[error("colors.toml: {0} is not a #rrggbb color")]
    Color(&'static str),
    #[error("colors.toml: mode must be \"light\" or \"dark\"")]
    Mode,
    #[error("colors.toml: the foreground does not read on the background")]
    Unreadable,
}

pub(super) fn current(home: &Path) -> Option<PathBuf> {
    let present = |path: &Path| path.symlink_metadata().is_ok();
    let modern = home.join(".local/state/omarchy");
    if present(&modern) {
        return Some(modern.join("current"));
    }
    let legacy = home.join(".config/omarchy/current");
    present(&legacy).then_some(legacy)
}

pub(super) fn read(home: &Path) -> Result<Palette, Error> {
    let current = current(home).ok_or(Error::NotInstalled)?;
    parse(&std::fs::read_to_string(current.join("theme/colors.toml"))?)
}

pub(super) fn parse(text: &str) -> Result<Palette, Error> {
    let table: toml::Table = text.parse()?;
    let color = |keys: &[&'static str]| -> Result<Option<Rgb>, Error> {
        for key in keys {
            if let Some(value) = table.get(*key) {
                return value
                    .as_str()
                    .and_then(Rgb::parse)
                    .map(Some)
                    .ok_or(Error::Color(key));
            }
        }
        Ok(None)
    };
    let required = |keys: &[&'static str]| color(keys)?.ok_or(Error::Missing(keys[0]));
    let background = required(&["background"])?;
    let foreground = required(&["foreground"])?;
    let accent = required(&["accent"])?;
    let red = required(&["red", "color1"])?;
    let green = required(&["green", "color2"])?;
    let yellow = required(&["yellow", "color3"])?;
    let blue = color(&["blue", "color4"])?.unwrap_or(accent);
    let magenta = color(&["magenta", "color5"])?.unwrap_or(accent);
    let cyan = color(&["cyan", "color6"])?.unwrap_or(blue);
    let border = color(&["muted", "color8"])?.unwrap_or(background.mix(foreground, 0.18));
    let surface = color(&["dark_background"])?.unwrap_or(background.mix(foreground, 0.04));
    let raised = color(&["lighter_background"])?.unwrap_or(background.mix(foreground, 0.07));
    let selection =
        color(&["selection", "selection_background"])?.unwrap_or(background.mix(accent, 0.25));
    let readable = |c: &Rgb| background.contrast(*c) >= 4.5;
    let muted_foreground = color(&["dark_foreground"])?
        .filter(readable)
        .or(Some(foreground.mix(background, 0.3)).filter(readable))
        .unwrap_or(foreground);
    if background.contrast(foreground) < 3. {
        return Err(Error::Unreadable);
    }
    let mode = match table.get("mode") {
        None if background.luminance() < foreground.luminance() => ThemeMode::Dark,
        None => ThemeMode::Light,
        Some(value) => match value.as_str() {
            Some("dark") => ThemeMode::Dark,
            Some("light") => ThemeMode::Light,
            _ => return Err(Error::Mode),
        },
    };
    Ok(Palette {
        mode,
        background,
        surface,
        raised,
        foreground,
        muted_foreground,
        border,
        accent,
        selection,
        red,
        yellow,
        green,
        blue,
        magenta,
        cyan,
    })
}

/// Watches every directory on the way to the palette, so creating Omarchy,
/// swapping its theme directory, or retargeting a symlink all wake the
/// follower. inotify watches inodes, so [`Watch::refresh`] re-registers any
/// path whose inode changed.
pub(super) struct Watch {
    home: PathBuf,
    watcher: notify::RecommendedWatcher,
    watched: BTreeMap<(u64, u64), PathBuf>,
}

impl Watch {
    pub(super) fn new(home: PathBuf) -> notify::Result<(Self, mpsc::Receiver<()>)> {
        let (wake, wakes) = mpsc::channel(1);
        let branches = [
            (home.clone(), &[".local", ".config"][..]),
            (home.join(".local"), &["state"][..]),
            (home.join(".local/state"), &["omarchy"][..]),
            (home.join(".config"), &["omarchy"][..]),
        ];
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok_and(|event| relevant(&event, &branches)) {
                let _ = wake.try_send(());
            }
        })?;
        let mut watch = Self {
            home,
            watcher,
            watched: BTreeMap::new(),
        };
        watch.refresh();
        Ok((watch, wakes))
    }

    pub(super) fn home(&self) -> &Path {
        &self.home
    }

    pub(super) fn refresh(&mut self) {
        let wanted = self.wanted();
        let stale: Vec<_> = self
            .watched
            .iter()
            .filter(|(id, path)| wanted.get(*id) != Some(*path))
            .map(|(id, path)| (*id, path.clone()))
            .collect();
        for (id, path) in stale {
            // A replaced inode already lost its watch; the error says so.
            let _ = self.watcher.unwatch(&path);
            self.watched.remove(&id);
        }
        for (id, path) in wanted {
            if self.watched.contains_key(&id) {
                continue;
            }
            if self
                .watcher
                .watch(&path, notify::RecursiveMode::NonRecursive)
                .is_ok()
            {
                self.watched.insert(id, path);
            }
        }
    }

    fn wanted(&self) -> BTreeMap<(u64, u64), PathBuf> {
        let mut dirs = vec![self.home.clone(), self.home.join(".local")];
        for (parent, root) in [
            (".local/state", ".local/state/omarchy"),
            (".config", ".config/omarchy"),
        ] {
            let root = self.home.join(root);
            let current = root.join("current");
            let theme = current.join("theme");
            dirs.extend([self.home.join(parent), root, current]);
            if let Ok(target) = std::fs::canonicalize(&theme) {
                dirs.push(target);
            }
            if let Some(parent) = std::fs::canonicalize(theme.join("colors.toml"))
                .ok()
                .and_then(|file| file.parent().map(Path::to_path_buf))
            {
                dirs.push(parent);
            }
            dirs.push(theme);
        }
        let mut wanted = BTreeMap::new();
        for dir in dirs {
            if let Ok(meta) = std::fs::metadata(&dir)
                && meta.is_dir()
            {
                wanted.entry((meta.dev(), meta.ino())).or_insert(dir);
            }
        }
        wanted
    }
}

fn relevant(event: &notify::Event, branches: &[(PathBuf, &[&str])]) -> bool {
    use notify::EventKind::{Access, Modify};
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    if event.need_rescan() {
        return true;
    }
    let writes = match event.kind {
        Access(AccessKind::Close(AccessMode::Write)) => true,
        Access(_) | Modify(ModifyKind::Metadata(_)) => false,
        _ => true,
    };
    writes
        && event.paths.iter().any(|path| {
            let branch = path
                .parent()
                .and_then(|dir| branches.iter().find(|(parent, _)| parent == dir));
            branch.is_none_or(|(_, children)| {
                path.file_name()
                    .is_some_and(|name| children.iter().any(|child| name == *child))
            })
        })
}

pub(super) fn follow(cx: &mut App) {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let watch = Watch::new(home.clone()).ok();
    if let Ok(palette) = read(&home) {
        super::install_omarchy(palette, cx);
    }
    let Some((mut watch, mut wakes)) = watch else {
        return;
    };
    let task = cx.spawn(async move |cx| {
        while wakes.recv().await.is_some() {
            let executor = cx.background_executor().clone();
            executor.timer(SETTLE).await;
            while wakes.try_recv().is_ok() {}
            let (returned, palette) = executor
                .spawn(async move {
                    watch.refresh();
                    let palette = read(watch.home());
                    (watch, palette)
                })
                .await;
            watch = returned;
            if let Ok(palette) = palette {
                cx.update(|cx| super::install_omarchy(palette, cx));
            }
        }
    });
    cx.global_mut::<super::Appearance>()._follow = Some(task);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appearance::Style;
    use std::fs;

    const TOKYO: &str = r##"
mode = "dark"
accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
dark_background = "#13141c"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#449dab"
blue = "#7aa2f7"
magenta = "#ad8ee6"
hyprland_active_border = "rgba(81a1c1ee) rgba(b48eadff) 45deg"
"##;

    const FLEXOKI: &str = r##"
mode = "light"
accent = "#205EA6"
selection = "#CECDC3"
muted = "#B7B5AC"
background = "#FFFCF0"
dark_background = "#f2efe4"
foreground = "#100F0F"
dark_foreground = "#878580"
red = "#D14D41"
yellow = "#D0A215"
green = "#879A39"
"##;

    const ANSI: &str = r##"
accent = "#ffd75f"
background = "#000000"
foreground = "#d0d0d0"
color1 = "#cc0000"
color2 = "#4e9a06"
color3 = "#c4a000"
color4 = "#3465a4"
color8 = "#555753"
"##;

    #[test]
    fn semantic_palettes_keep_their_explicit_mode() {
        let tokyo = parse(TOKYO).unwrap();
        assert_eq!(tokyo.mode, ThemeMode::Dark);
        assert_eq!(tokyo.surface, Rgb(0x13141c));
        assert_eq!(tokyo.selection, Rgb(0x292e42));
        assert_eq!(tokyo.border, Rgb(0x414868));
        let flexoki = parse(FLEXOKI).unwrap();
        assert_eq!(flexoki.mode, ThemeMode::Light);
        assert_eq!(flexoki.accent, Rgb(0x205ea6));
        let forced = parse(&TOKYO.replace("mode = \"dark\"", "mode = \"light\"")).unwrap();
        assert_eq!(forced.mode, ThemeMode::Light);
    }

    #[test]
    fn rounded_chrome_softens_lines_without_fading_text_or_controls() {
        for text in [TOKYO, FLEXOKI, ANSI] {
            let palette = parse(text).unwrap();
            let config = palette.config("Omarchy", Style::OMARCHY);
            let colors = &config.colors;
            let border = Rgb::parse(colors.border.as_deref().unwrap()).unwrap();
            assert!(
                palette.background.contrast(border) < palette.background.contrast(palette.border)
            );
            assert_eq!((config.radius, config.radius_lg), (Some(8), Some(12)));
            assert_eq!(config.mode, palette.mode);
            assert_eq!(
                colors.foreground.as_deref(),
                Some(palette.foreground.hex().as_str())
            );
            assert_eq!(
                colors.primary.as_deref(),
                Some(super::super::strong_fill(palette.accent).hex().as_str())
            );
            assert_eq!(
                colors.selection.as_deref(),
                Some(palette.accent.hex().as_str())
            );
            let track = Rgb::parse(colors.switch.as_deref().unwrap()).unwrap();
            assert!(
                palette.background.contrast(track) >= 1.5,
                "an off switch stays visible"
            );
            assert_eq!(
                colors.scrollbar_thumb.as_deref(),
                Some(palette.border.hex().as_str())
            );
        }
    }

    #[test]
    fn ansi_palettes_derive_their_mode_and_missing_roles() {
        let ansi = parse(ANSI).unwrap();
        assert_eq!(ansi.mode, ThemeMode::Dark);
        assert_eq!((ansi.red, ansi.blue), (Rgb(0xcc0000), Rgb(0x3465a4)));
        assert_eq!(ansi.border, Rgb(0x555753));
        assert_eq!(ansi.cyan, ansi.blue);
        let light = parse(
            &ANSI
                .replace("#000000", "#fafafa")
                .replace("#d0d0d0", "#202020"),
        );
        assert_eq!(light.unwrap().mode, ThemeMode::Light);
    }

    #[test]
    fn text_roles_stay_legible() {
        let tokyo = parse(&TOKYO.replace("#565f89", "#2a2b36")).unwrap();
        assert!(tokyo.background.contrast(tokyo.muted_foreground) >= 4.5);
        for palette in [parse(ANSI).unwrap(), parse(FLEXOKI).unwrap()] {
            assert!(super::super::strong_fill(palette.accent).contrast(super::super::WHITE) >= 4.5);
        }
    }

    #[test]
    fn broken_palettes_are_refused_whole() {
        let refused = |text: String| parse(&text).unwrap_err().to_string();
        assert!(refused(TOKYO.replace("background = \"#1a1b26\"\n", "")).contains("background"));
        assert!(refused(TOKYO.replace("\"#f7768e\"", "\"#f77\"")).contains("red"));
        assert!(refused(TOKYO.replace("\"#9ece6a\"", "12")).contains("green"));
        assert!(refused(TOKYO.replace("\"dark\"", "\"blue\"")).contains("mode"));
        assert!(refused(TOKYO.replace("#a9b1d6", "#1c1d29")).contains("read"));
        assert!(refused("background = ".into()).starts_with("colors.toml"));
    }

    struct Home(tempfile::TempDir);

    impl Home {
        fn new() -> Self {
            Self(tempfile::tempdir().unwrap())
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.path().join(rel)
        }

        fn write(&self, rel: &str, text: &str) {
            let path = self.path(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }

        fn read(&self) -> Result<Palette, Error> {
            read(self.0.path())
        }
    }

    #[test]
    fn the_modern_state_directory_wins_and_legacy_is_never_revived() {
        let home = Home::new();
        assert!(matches!(home.read(), Err(Error::NotInstalled)));
        home.write(".config/omarchy/current/theme/colors.toml", FLEXOKI);
        assert_eq!(home.read().unwrap().mode, ThemeMode::Light, "legacy alone");
        home.write(".local/state/omarchy/current/theme/colors.toml", TOKYO);
        assert_eq!(home.read().unwrap().mode, ThemeMode::Dark, "modern first");

        fs::remove_dir_all(home.path(".local/state/omarchy/current")).unwrap();
        assert!(home.read().is_err(), "a modern install without a theme");
        std::os::unix::fs::symlink(
            home.path("missing"),
            home.path(".local/state/omarchy/current"),
        )
        .unwrap();
        assert!(home.read().is_err(), "a dangling modern current");
    }

    async fn woke(wakes: &mut mpsc::Receiver<()>) -> bool {
        let woke = tokio::time::timeout(Duration::from_secs(3), wakes.recv())
            .await
            .is_ok();
        tokio::time::sleep(Duration::from_millis(100)).await;
        while wakes.try_recv().is_ok() {}
        woke
    }

    #[tokio::test]
    async fn an_install_after_startup_in_an_empty_home_is_followed() {
        let home = Home::new();
        let (mut watch, mut wakes) = Watch::new(home.0.path().to_path_buf()).unwrap();
        home.write("notes.txt", "unrelated");
        assert!(!woke(&mut wakes).await, "unrelated home writes are ignored");

        home.write(".config/omarchy/current/theme/colors.toml", FLEXOKI);
        assert!(woke(&mut wakes).await, "creating a legacy install wakes");
        watch.refresh();
        assert_eq!(home.read().unwrap().mode, ThemeMode::Light);

        home.write(".local/state/omarchy/current/theme/colors.toml", TOKYO);
        assert!(woke(&mut wakes).await, "creating a modern install wakes");
        watch.refresh();
        assert_eq!(home.read().unwrap().mode, ThemeMode::Dark);

        home.write(".local/state/omarchy/current/theme/colors.toml", FLEXOKI);
        assert!(woke(&mut wakes).await, "the new palette remains watched");
        assert_eq!(home.read().unwrap().mode, ThemeMode::Light);
    }

    #[tokio::test]
    async fn a_swapped_theme_directory_is_followed_and_keeps_being_watched() {
        let home = Home::new();
        home.write(".local/state/omarchy/current/theme/colors.toml", TOKYO);
        let (mut watch, mut wakes) = Watch::new(home.0.path().to_path_buf()).unwrap();

        home.write(
            ".local/state/omarchy/current/next-theme/colors.toml",
            FLEXOKI,
        );
        fs::remove_dir_all(home.path(".local/state/omarchy/current/theme")).unwrap();
        fs::rename(
            home.path(".local/state/omarchy/current/next-theme"),
            home.path(".local/state/omarchy/current/theme"),
        )
        .unwrap();
        assert!(woke(&mut wakes).await, "the swap wakes");
        watch.refresh();
        assert_eq!(home.read().unwrap().mode, ThemeMode::Light);

        home.write(".local/state/omarchy/current/theme/colors.toml", "broken =");
        assert!(woke(&mut wakes).await, "an edit in the new directory wakes");
        watch.refresh();
        assert!(home.read().is_err(), "a bad palette is refused");
        home.write(".local/state/omarchy/current/theme/colors.toml", TOKYO);
        assert!(woke(&mut wakes).await, "the fix wakes");
        assert_eq!(home.read().unwrap().mode, ThemeMode::Dark, "and recovers");
    }

    #[tokio::test]
    async fn a_retargeted_theme_symlink_is_followed() {
        let home = Home::new();
        home.write("themes/tokyo/colors.toml", TOKYO);
        home.write("themes/flexoki/colors.toml", FLEXOKI);
        fs::create_dir_all(home.path(".local/state/omarchy/current")).unwrap();
        let link = home.path(".local/state/omarchy/current/theme");
        std::os::unix::fs::symlink(home.path("themes/tokyo"), &link).unwrap();
        let (mut watch, mut wakes) = Watch::new(home.0.path().to_path_buf()).unwrap();

        let next = home.path(".local/state/omarchy/current/theme.next");
        std::os::unix::fs::symlink(home.path("themes/flexoki"), &next).unwrap();
        fs::rename(&next, &link).unwrap();
        assert!(woke(&mut wakes).await, "retargeting wakes");
        watch.refresh();
        assert_eq!(home.read().unwrap().mode, ThemeMode::Light);

        home.write(
            "themes/flexoki/colors.toml",
            &FLEXOKI.replace("#205EA6", "#3a6ea5"),
        );
        assert!(woke(&mut wakes).await, "editing the new target wakes");
        assert_eq!(home.read().unwrap().accent, Rgb(0x3a6ea5));

        home.write("themes/tokyo/colors.toml", FLEXOKI);
        assert!(!woke(&mut wakes).await, "the old target no longer wakes");
    }

    #[tokio::test]
    async fn reading_the_palette_does_not_wake() {
        let home = Home::new();
        home.write(".local/state/omarchy/current/theme/colors.toml", TOKYO);
        let (_watch, mut wakes) = Watch::new(home.0.path().to_path_buf()).unwrap();
        home.read().unwrap();
        assert!(!woke(&mut wakes).await);
    }
}
