use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read as _};
use std::ops::Range;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Component, Path};

use markdown::mdast::Node;
use uuid::Uuid;

use crate::app::messages::{
    Attachment, MAX_ATTACHMENT_BYTES, MAX_MESSAGE_FILES, MessagePart, flatten, validate_name,
};

pub(crate) struct OutputImage {
    pub file: Attachment,
    pub data: Vec<u8>,
}

pub(crate) struct ImageReply {
    pub text: String,
    pub parts: Vec<MessagePart>,
    pub images: Vec<OutputImage>,
}

pub(crate) fn prepare(project: &Path, text: &str) -> Option<ImageReply> {
    if !text.contains("![") {
        return None;
    }
    let tree = markdown::to_mdast(text, &markdown::ParseOptions::default()).ok()?;
    let mut definitions = HashMap::new();
    definitions_in(&tree, &mut definitions);
    let mut candidates = Vec::new();
    images_in(&tree, &definitions, &mut candidates);
    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(project)
        .ok()?;
    let mut images = Vec::new();
    let mut saved = HashMap::<std::path::PathBuf, Attachment>::new();
    let mut parts = Vec::new();
    let mut offset = 0;
    for (range, url) in candidates {
        let Ok(path) = percent_encoding::percent_decode_str(&url).decode_utf8() else {
            continue;
        };
        let Some(relative) = relative_path(project, &path) else {
            continue;
        };
        let key: std::path::PathBuf = relative
            .components()
            .filter(|component| *component != Component::CurDir)
            .collect();
        let file = if let Some(file) = saved.get(&key) {
            file.clone()
        } else {
            if images.len() == MAX_MESSAGE_FILES {
                continue;
            }
            let Some(image) = read_image(&root, &key) else {
                continue;
            };
            let file = image.file.clone();
            saved.insert(key, file.clone());
            images.push(image);
            file
        };
        if offset < range.start {
            parts.push(MessagePart::Text {
                text: text[offset..range.start].to_owned(),
            });
        }
        offset = range.end;
        parts.push(MessagePart::File { file });
    }
    if images.is_empty() {
        return None;
    }
    if offset < text.len() {
        parts.push(MessagePart::Text {
            text: text[offset..].to_owned(),
        });
    }
    Some(ImageReply {
        text: flatten(&parts),
        parts,
        images,
    })
}

fn definitions_in(node: &Node, definitions: &mut HashMap<String, String>) {
    if let Node::Definition(definition) = node {
        definitions
            .entry(definition.identifier.clone())
            .or_insert_with(|| definition.url.clone());
    }
    for child in node.children().into_iter().flatten() {
        definitions_in(child, definitions);
    }
}

fn images_in(
    node: &Node,
    definitions: &HashMap<String, String>,
    images: &mut Vec<(Range<usize>, String)>,
) {
    let url = match node {
        Node::Image(image) => Some(&image.url),
        Node::ImageReference(image) => definitions.get(&image.identifier),
        _ => None,
    };
    if let (Some(url), Some(position)) = (url, node.position()) {
        images.push((position.start.offset..position.end.offset, url.clone()));
    }
    for child in node.children().into_iter().flatten() {
        images_in(child, definitions, images);
    }
}

fn relative_path<'a>(project: &'a Path, url: &'a str) -> Option<&'a Path> {
    if reqwest::Url::parse(url).is_ok() {
        return None;
    }
    if let Some(path) = url.strip_prefix("~/work/") {
        return Some(Path::new(path));
    }
    let path = Path::new(url);
    if !path.is_absolute() {
        return Some(path);
    }
    [
        project,
        Path::new(crate::computers::GUEST_PROJECT),
        Path::new(crate::computers::ROOT_PROJECT),
    ]
    .into_iter()
    .find_map(|root| path.strip_prefix(root).ok())
}

fn open_beneath(root: &File, path: &Path) -> Option<File> {
    let components: Vec<_> = path.components().collect();
    let mut directory = root.try_clone().ok()?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(component) = component else {
            if *component == Component::CurDir {
                continue;
            }
            return None;
        };
        let last = index == components.len() - 1;
        let name = CString::new(component.as_bytes()).ok()?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last {
                libc::O_NONBLOCK
            } else {
                libc::O_DIRECTORY
            };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return None;
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Some(directory)
}

fn read_image(root: &File, path: &Path) -> Option<OutputImage> {
    let name = path.file_name()?.to_str()?;
    validate_name(name).ok()?;
    let directory = open_beneath(root, path)?;
    let metadata = directory.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES || metadata.nlink() != 1 {
        return None;
    }
    let mut data = Vec::new();
    directory
        .take(MAX_ATTACHMENT_BYTES + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(Cursor::new(&data))
        .with_guessed_format()
        .ok()?;
    let media_type = match reader.format()? {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().ok()?;
    Some(OutputImage {
        file: Attachment {
            id: Uuid::new_v4(),
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            size: data.len() as u64,
        },
        data,
    })
}
