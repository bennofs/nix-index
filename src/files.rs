//! Data types for working with trees of files.
//!
//! The main type here is `FileTree` which represents
//! such as the file listing for a store path.
use std::collections::HashMap;
use serde_bytes::ByteBuf;
use std::str;
use std::io::{self, Write};
use memchr::memchr;

use crate::frcode;

/// This enum represents a single node in a file tree.
///
/// The type is generic over the contents of a directory node,
/// because we want to use this enum to represent both a flat
/// structure where a directory only stores some meta-information about itself
/// (such as the number of children) and full file trees, where a
/// directory contains all the child nodes.
///
/// Note that file nodes by themselves do not have names. Names are given
/// to file nodes by the parent directory, which has a map of entry names to
/// file nodes.
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub enum FileNode<T> {
    /// A regular file. This is the normal kind of file which is
    /// neither a directory not a symlink.
    Regular {
        /// The size of this file, in bytes.
        size: u64,
        /// Whether or not this file has the `executable` bit set.
        executable: bool,
    },
    /// A symbolic link that points to another file path.
    Symlink {
        /// The path that this symlink points to.
        target: ByteBuf,
    },
    /// A directory. It usually has a mapping of names to child nodes (in
    /// the case of a fill tree), but we also support a reduced form where
    /// we only store the number of entries in the directory.
    Directory {
        /// The size of a directory is the number of children it contains.
        size: u64,

        /// The contents of this directory. These are generic, as explained
        /// in the documentation for this type.
        contents: T,
    },
}

/// The type of a file.
///
/// This mirrors the variants of `FileNode`, but without storing
/// data in each variant.
///
/// An exception to this is the `executable` field for the regular type.
/// This is needed since we present `regular` and `executable` files as different
/// to the user, so we need a way to represent both types.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileType {
    Regular { executable: bool },
    Directory,
    Symlink,
}

/// This lists all file types that can currently be represented.
pub const ALL_FILE_TYPES: &'static [FileType] = &[
    FileType::Regular { executable: true },
    FileType::Regular { executable: false },
    FileType::Directory,
    FileType::Symlink,
];

impl<T> FileNode<T> {
    /// Split this node into a node without contents and optionally the contents themselves,
    /// if the node was a directory.
    pub fn split_contents(&self) -> (FileNode<()>, Option<&T>) {
        use self::FileNode::*;
        match *self {
            Regular { size, executable } => {
                (
                    Regular {
                        size: size,
                        executable: executable,
                    },
                    None,
                )
            }
            Symlink { ref target } => (Symlink { target: target.clone() }, None),
            Directory { size, ref contents } => {
                (
                    Directory {
                        size: size,
                        contents: (),
                    },
                    Some(contents),
                )
            }
        }
    }

    /// Return the type of this file.
    pub fn get_type(&self) -> FileType {
        match *self {
            FileNode::Regular { executable, .. } => FileType::Regular { executable: executable },
            FileNode::Directory { .. } => FileType::Directory,
            FileNode::Symlink { .. } => FileType::Symlink,
        }
    }
}

impl FileNode<()> {
    fn encode<W: Write>(&self, encoder: &mut frcode::Encoder<W>) -> io::Result<()> {
        use self::FileNode::*;
        match *self {
            Regular { executable, size } => {
                let e = if executable { "x" } else { "r" };
                encoder.write_meta(format!("{}{}", size, e).as_bytes())?;
            }
            Symlink { ref target } => {
                encoder.write_meta(target)?;
                encoder.write_meta(b"s")?;
            }
            Directory { size, contents: () }=> {
                encoder.write_meta(format!("{}d", size).as_bytes())?;
            }
        }
        Ok(())
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        use self::FileNode::*;
        buf.split_last().and_then(|(kind, buf)| match *kind {
            b'x' | b'r' => {
                let executable = *kind == b'x';
                str::from_utf8(buf).ok().and_then(|s| s.parse().ok()).map(
                    |size| {
                        Regular {
                            executable: executable,
                            size: size,
                        }
                    },
                )
            }
            b's' => Some(Symlink { target: ByteBuf::from(buf) }),
            b'd' => {
                str::from_utf8(buf).ok().and_then(|s| s.parse().ok()).map(
                    |size| {
                        Directory {
                            size: size,
                            contents: (),
                        }
                    },
                )
            }
            _ => None,
        })
    }
}

/// This type represents a full tree of files.
///
/// A *file tree* is a *file node* where each directory contains
/// the tree for its children.
#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
pub struct FileTree(FileNode<HashMap<ByteBuf, FileTree>>);

/// An entry in a file tree is a path to a node paired with that node.
///
/// If the entry refers to a directory, it only stores information about that
/// directory itself. It does not contain the children of the directory.
pub struct FileTreeEntry {
    pub path: Vec<u8>,
    pub node: FileNode<()>,
}

impl FileTreeEntry {
    pub fn encode<W: Write>(self, encoder: &mut frcode::Encoder<W>) -> io::Result<()> {
        self.node.encode(encoder)?;
        encoder.write_path(self.path)?;
        Ok(())
    }

    pub fn decode(buf: &[u8]) -> Option<FileTreeEntry> {
        memchr(b'\0', buf).and_then(|sep| {
            let path = &buf[(sep + 1)..];
            let node = &buf[0..sep];
            FileNode::decode(node).map(|node| {
                FileTreeEntry {
                    path: path.to_vec(),
                    node: node,
                }
            })
        })
    }
}

impl FileTree {
    pub fn regular(size: u64, executable: bool) -> Self {
        FileTree(FileNode::Regular {
            size: size,
            executable: executable,
        })
    }

    pub fn symlink(target: ByteBuf) -> Self {
        FileTree(FileNode::Symlink { target: target })
    }

    pub fn directory(entries: HashMap<ByteBuf, FileTree>) -> Self {
        FileTree(FileNode::Directory {
            size: entries.len() as u64,
            contents: entries,
        })
    }

    pub fn to_list(&self) -> Vec<FileTreeEntry> {
        let mut result = Vec::new();

        let mut stack = Vec::with_capacity(16);
        stack.push((Vec::new(), self));

        while let Some(entry) = stack.pop() {
            let path = entry.0;
            let &FileTree(ref current) = entry.1;
            let (node, contents) = current.split_contents();
            if let Some(entries) = contents {
                let mut entries = entries.iter().collect::<Vec<_>>();
                entries.sort_by(|a, b| Ord::cmp(a.0, b.0));
                for (name, entry) in entries {
                    let mut path = path.clone();
                    path.push(b'/');
                    path.extend_from_slice(name);
                    stack.push((path, entry));
                }
            }
            result.push(FileTreeEntry {
                path: path,
                node: node,
            });
        }
        result
    }
}
