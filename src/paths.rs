use std::{
    io::{self, ErrorKind}, path::{Path, PathBuf}
};
use thiserror::Error;

use crate::paths::SubDirectoryResult::IsNotDirectory;

// File exists and is absolute and normalized
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct CanonicalDirBuf(PathBuf);

impl AsRef<Path> for CanonicalDirBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

pub enum SubDirectoryResult {
    IsNotDirectory,
    SubDirectory(CanonicalDirBuf)
}

impl SubDirectoryResult {
    pub fn expect_dir(self, s: &str) -> CanonicalDirBuf {
        match self {
            Self::IsNotDirectory => panic!("{}", s),
            Self::SubDirectory(x) => x
        }
    }
}

impl CanonicalDirBuf {
    pub fn from_absolute(path: &Path, description: &str) -> Result<Self, PathError> {
        check_absolute(path, description)?;
        match path.canonicalize() {
            Ok(path) => Ok(Self(path)),
            Err(source) => match source.kind() {
                ErrorKind::NotFound => Err(PathError::NotFound {
                    description: description.to_owned(),
                    path: path.to_owned(),
                }),
                _ => Err(PathError::GeneralIOError {
                    path: path.to_owned(),
                    source,
                }),
            },
        }
    }

    pub fn try_from_existing_subdirectory(&self, relative: &RelativePathBuf) -> Result<SubDirectoryResult, PathError> {
        let inner = self.0.join(relative.as_ref());
        let metadata = inner.metadata().map_err(|source| PathError::GeneralIOError {
            path: inner.to_owned(),
            source,
        })?;
        if metadata.is_dir() {
            Ok(SubDirectoryResult::SubDirectory(Self(inner)))
        } else {
            Ok(SubDirectoryResult::IsNotDirectory)
        }
    }
}

// File is not absolute, do not contain . or .. and may not exist
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct RelativePathBuf(PathBuf);

impl RelativePathBuf {
    pub fn new(path: &Path) -> Option<Self> {
        if path.is_absolute() {
            return None
        }

        if !path.components().all(|c| matches!(c, std::path::Component::Normal(_))) {
            return None
        }

        return Some(RelativePathBuf(path.to_owned()))
    }

    pub fn parent(&self) -> Option<Self> {
        self.0.parent().and_then(|x| {
            // Not sure why this can happen, but an LLM guarded against this, so just to be sure
            if x.as_os_str().is_empty() {
                panic!("Two-component RelativePathBuf should never have empty path");
            } else {
                Some(Self(x.to_owned()))
            }
        })
    }
}

impl AsRef<Path> for RelativePathBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl From<&std::fs::DirEntry> for RelativePathBuf {
    fn from(item: &std::fs::DirEntry) -> Self {
        // Safety: The file name of an entry is always a Component::Normal (I think)
        Self::new(Path::new(&item.file_name())).unwrap()
    }
}

// File may not exist, but is absolute and normalized.
// Its parent is caonical
#[derive(Debug)]
pub struct CanonicalChildFileBuf(PathBuf);

impl AsRef<Path> for CanonicalChildFileBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl CanonicalChildFileBuf {
    pub fn from_absolute(path: &Path, description: &str) -> Result<Self, PathError> {
        check_absolute(path, description)?;

        let file_name = path
            .file_name()
            .ok_or_else(|| PathError::BadChildDirectory {
                description: description.to_owned(),
                path: path.to_owned(),
            })?;

        // Check it has a parent
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| PathError::BadChildDirectory {
                description: description.to_owned(),
                path: path.to_owned(),
            })?;

        // Check parent is canonical
        let canonical_parent = CanonicalDirBuf::from_absolute(parent, description)?;

        let full_path = canonical_parent.0.join(file_name);

        if full_path.exists() && !full_path.is_file() {
            Err(PathError::IsNotFileOrMissing { description: description.to_owned(), path: full_path })
        } else {
            Ok(Self(full_path))
        }
    }

    pub fn parent(&self) -> CanonicalDirBuf {
        // This should never fail, because the invariants of CanonicalDirBuf
        // tests that the parent exists and is a canonical directory
        CanonicalDirBuf(self.0.parent().unwrap().to_owned())
    }

}

fn check_absolute(path: &Path, description: &str) -> Result<(), PathError> {
    if !path.is_absolute() {
        return Err(PathError::NotAbsolute {
            description: description.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(())
}


#[derive(Debug, Error)]
pub enum PathError {
    #[error("Got an unexpected IO error.\nPath: {}\nError: {source}", path.display())]
    GeneralIOError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    
    #[error("{description} must be an absolute path, but it was {}", path.display())]
    NotAbsolute { description: String, path: PathBuf },

    #[error("{description} at path {} must exist, but was not found", path.display())]
    NotFound { description: String, path: PathBuf },

    #[error("{description} must be a resolved, canonical path with an existing parent.\n\
        It cannot be empty, or the root directory, nor end with '..'. Found: '{}'", path.display())]
    BadChildDirectory { description: String, path: PathBuf },

    #[error("{description} at path {} must be a file or must not exist,\n\
        but is not a normal file (i.e. maybe a directory?)", path.display())]
    IsNotFileOrMissing {description: String, path: PathBuf}
}
