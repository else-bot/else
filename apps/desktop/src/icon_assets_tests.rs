use super::{AppAssets, AssetSource};
use gpui_kit::assets::IconName;
use proc_macro2::{TokenStream, TokenTree};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

fn source_files(dir: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(dir).expect("read desktop source directory") {
        let path = entry.expect("read desktop source entry").path();
        if path.is_dir() {
            source_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

fn tokens(stream: TokenStream) -> Vec<TokenTree> {
    let mut result = Vec::new();
    for token in stream {
        result.push(token.clone());
        if let TokenTree::Group(group) = &token {
            result.extend(tokens(group.stream()));
        }
    }
    result
}

fn icon_references(source: &[TokenTree], types: &BTreeSet<String>) -> BTreeSet<String> {
    let mut used = BTreeSet::new();
    for window in source.windows(4) {
        let [
            TokenTree::Ident(ty),
            TokenTree::Punct(a),
            TokenTree::Punct(b),
            variant,
        ] = window
        else {
            continue;
        };
        if !types.contains(&ty.to_string()) || a.as_char() != ':' || b.as_char() != ':' {
            continue;
        }
        match variant {
            TokenTree::Ident(name) => {
                used.insert(name.to_string());
            }
            TokenTree::Group(group) => {
                let imports = tokens(group.stream());
                assert_no_glob(&imports);
                used.extend(imports.into_iter().filter_map(|token| {
                    if let TokenTree::Ident(name) = token {
                        Some(name.to_string())
                    } else {
                        None
                    }
                }));
            }
            TokenTree::Punct(punct) if punct.as_char() == '*' => {
                assert_no_glob(std::slice::from_ref(variant));
            }
            _ => {}
        }
    }
    used
}

fn assert_no_glob(source: &[TokenTree]) {
    assert!(
        !source
            .iter()
            .any(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '*')),
        "Use explicit IconName variants instead of a glob import so the bundle check can audit them"
    );
}

fn icon_type_names(sources: &[Vec<TokenTree>]) -> BTreeSet<String> {
    let mut types = BTreeSet::from(["IconName".to_owned()]);
    loop {
        let before = types.len();
        for source in sources {
            for window in source.windows(3) {
                if let [
                    TokenTree::Ident(name),
                    TokenTree::Ident(keyword),
                    TokenTree::Ident(alias),
                ] = window
                    && types.contains(&name.to_string())
                    && keyword == "as"
                {
                    types.insert(alias.to_string());
                }
            }
            for (index, window) in source.windows(3).enumerate() {
                if let [
                    TokenTree::Ident(keyword),
                    TokenTree::Ident(alias),
                    TokenTree::Punct(eq),
                ] = window
                    && keyword == "type"
                    && eq.as_char() == '='
                {
                    let target = source[index + 3..]
                            .iter()
                            .take_while(|token| {
                                !matches!(token, TokenTree::Punct(punct) if punct.as_char() == ';')
                            })
                            .filter_map(|token| match token {
                                TokenTree::Ident(name) => Some(name.to_string()),
                                _ => None,
                            })
                            .last();
                    if target.is_some_and(|target| types.contains(&target)) {
                        types.insert(alias.to_string());
                    }
                }
            }
        }
        if types.len() == before {
            return types;
        }
    }
}

#[test]
fn brand_icons_load_and_list_as_svg() {
    let bytes = AppAssets
        .load(super::DISCORD_ICON)
        .expect("load the Discord icon")
        .expect("the Discord icon is bundled");
    assert!(std::str::from_utf8(&bytes).is_ok_and(|svg| svg.starts_with("<svg")));
    let listed = AppAssets.list("icons/brand/").expect("list brand icons");
    assert!(
        listed
            .iter()
            .any(|path| path.as_ref() == super::DISCORD_ICON)
    );
}

#[test]
fn every_desktop_icon_has_bundled_svg_bytes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    source_files(&root, &mut files);
    files.sort();
    let sources: Vec<_> = files
        .iter()
        .map(|path| {
            let source = fs::read_to_string(path).expect("read desktop Rust source");
            let stream = source.parse().expect("tokenize desktop Rust source");
            tokens(stream)
        })
        .collect();
    let types = icon_type_names(&sources);
    let icons: BTreeMap<_, _> = IconName::ALL
        .iter()
        .map(|icon| (format!("{icon:?}"), icon))
        .collect();
    let mut checked = BTreeSet::new();
    let mut missing = Vec::new();
    for (file, source) in files.iter().zip(sources) {
        let used = icon_references(&source, &types);
        for name in used {
            let Some(icon) = icons.get(&name) else {
                continue;
            };
            checked.insert(name.clone());
            let path = icon.path();
            if !matches!(AppAssets.load(&path), Ok(Some(bytes)) if !bytes.is_empty()) {
                missing.push(format!("{}: IconName::{name} ({path})", file.display()));
            }
        }
    }
    assert!(!checked.is_empty(), "no desktop icon references were found");
    assert!(
        missing.is_empty(),
        "Desktop icons have no bundled SVG bytes. Add them to AppIcons in apps/desktop/src/main.rs:\n{}",
        missing.join("\n")
    );
}

#[test]
fn icon_scan_handles_spacing_imports_and_macros_without_reading_comments_or_strings() {
    let source = r#"
        // IconName::CloudUpload
        let text = "IconName::Pencil";
        let icon = gpui_kit::assets::IconName :: Plug;
        example!(IconName::Lock);
        use IconName::{ChevronRight, ChevronDown};
        use gpui_kit::assets::IconName as Icons;
        let aliased = Icons::Menu;
        type OtherIcons = gpui_kit::assets::IconName;
        let type_aliased = OtherIcons::Send;
    "#;
    let sources = vec![tokens(source.parse().unwrap())];
    let used = icon_references(&sources[0], &icon_type_names(&sources));
    assert_eq!(
        used,
        BTreeSet::from_iter(
            [
                "Plug",
                "Lock",
                "ChevronRight",
                "ChevronDown",
                "Menu",
                "Send"
            ]
            .map(str::to_owned)
        )
    );
}

#[test]
#[should_panic(expected = "Use explicit IconName variants")]
fn icon_glob_imports_cannot_bypass_the_bundle_check() {
    let source = tokens("use IconName::*;".parse().unwrap());
    icon_references(&source, &icon_type_names(std::slice::from_ref(&source)));
}
