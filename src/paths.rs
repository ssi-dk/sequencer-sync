use serde::{Deserialize, Serialize};
use std::{
    borrow::Borrow,
    ffi::{OsStr, OsString},
    fs::DirEntry,
    io::{Error, ErrorKind},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;

#[derive(Debug, Hash, PartialEq, Eq, Deserialize, Serialize, Clone)]
pub struct NormalPathSegmentBuf(OsString);

impl From<&std::fs::DirEntry> for NormalPathSegmentBuf {
    fn from(item: &std::fs::DirEntry) -> Self {
        // Safety: The file name of an entry is always a Component::Normal (I think)
        NormalPathSegmentBuf(item.file_name())
    }
}

impl AsRef<OsStr> for NormalPathSegment {
    fn as_ref(&self) -> &OsStr {
        &self.0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
pub struct NormalUTF8Segment(String);

impl NormalUTF8Segment {
    pub fn to_inner(self) -> String {
        self.0
    }

    pub fn from_timestamp(tag: &str) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let s = format!(
            ".sequencer-sync-{tag}-write-probe-{}-{timestamp}",
            std::process::id()
        );
        Self(s)
    }

    pub fn as_normal(&self) -> &NormalPathSegment {
        // Safety: Self's invariants are a superset of NormalPathSegment
        unsafe { NormalPathSegment::from_os_str(OsStr::from_bytes(self.0.as_bytes())) }
    }
}

pub enum DirEntrySegmentCases {
    UTF8Segment(NormalUTF8Segment),
    IOError(std::io::Error),
    IsRoot,
    NotUTF8,
    IsSymlink,
}

pub fn segment(entry: &DirEntry) -> DirEntrySegmentCases {
    let md = match entry.metadata() {
        Ok(md) => md,
        Err(e) => return DirEntrySegmentCases::IOError(e),
    };
    if md.is_symlink() {
        return DirEntrySegmentCases::IsSymlink;
    }
    let file_name = entry.file_name();
    if file_name == "/" {
        return DirEntrySegmentCases::IsRoot;
    }
    NormalPathSegment::new(Path::new(&file_name)).unwrap_or_else(|| {
        panic!("DirEntry segment ought to be a Normal segment, got {:?file_name}")
    });
    match file_name.into_string() {
        Ok(s) => DirEntrySegmentCases::UTF8Segment(NormalUTF8Segment(s)),
        Err(_) => DirEntrySegmentCases::NotUTF8,
    }
}

impl From<NormalUTF8Segment> for NormalPathSegmentBuf {
    fn from(value: NormalUTF8Segment) -> Self {
        NormalPathSegmentBuf(value.to_inner().into())
    }
}

impl<'a> From<&'a NormalPathSegment> for &'a Path {
    fn from(value: &'a NormalPathSegment) -> Self {
        Path::new(&value.0)
    }
}

impl TryFrom<&NormalPathSegment> for NormalUTF8Segment {
    type Error = ();

    fn try_from(value: &NormalPathSegment) -> Result<Self, Self::Error> {
        value.0.to_str().map(|s| Self(s.to_owned())).ok_or(())
    }
}

// Not the file root, not the Windows drive, not .., not ., and
// no path separator in this.
#[repr(transparent)]
pub struct NormalPathSegment(OsStr);

impl NormalPathSegment {
    pub fn new(path: &Path) -> Option<&Self> {
        let mut components = path.components();
        match components.next() {
            None => return None,
            Some(std::path::Component::Normal(_)) => (),
            Some(_) => return None,
        }
        if components.next().is_some() {
            return None;
        }
        unsafe { return Some(Self::from_os_str(path.as_os_str())) }
    }

    unsafe fn from_os_str(s: &OsStr) -> &Self {
        // SAFETY: `MyOsStr` has the same layout as `OsStr`
        unsafe { &*(s as *const OsStr as *const Self) }
    }
}

impl ToOwned for NormalPathSegment {
    type Owned = NormalPathSegmentBuf;

    fn to_owned(&self) -> Self::Owned {
        NormalPathSegmentBuf(self.0.to_owned())
    }
}

impl Borrow<NormalPathSegment> for NormalPathSegmentBuf {
    fn borrow(&self) -> &NormalPathSegment {
        unsafe { NormalPathSegment::from_os_str(&self.0) }
    }
}

impl std::ops::Deref for NormalPathSegmentBuf {
    type Target = NormalPathSegment;

    fn deref(&self) -> &NormalPathSegment {
        unsafe { NormalPathSegment::from_os_str(&self.0) }
    }
}

// File exists and is absolute and normalized
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct CanonicalDirBuf(PathBuf);

impl AsRef<Path> for CanonicalDirBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

pub enum SubDirectoryResult {
    IsNotDirectory,
    SubDirectory(CanonicalDirBuf),
}

impl SubDirectoryResult {
    pub fn expect_dir(self, s: &str) -> CanonicalDirBuf {
        match self {
            Self::IsNotDirectory => panic!("{}", s),
            Self::SubDirectory(x) => x,
        }
    }
}

impl CanonicalDirBuf {
    pub fn create_subdir(&self, subdir: &NormalPathSegment) -> Result<Self, PathError> {
        let path = self.as_ref().join(subdir.as_ref());
        match std::fs::create_dir(&path) {
            Ok(()) => {
                // Guaranteed to work, since we just created the directory
                // and the normal segment is already normalized
                Ok(CanonicalDirBuf(path))
            }
            Err(source) => Err(PathError::GeneralIOError {
                path: path.to_owned(),
                source,
            }),
        }
    }

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

    pub fn try_from_existing_subdirectory(
        &self,
        relative: &NormalPathSegment,
    ) -> Result<SubDirectoryResult, PathError> {
        let inner = self.0.join(Path::new(&relative.0));
        let metadata = inner
            .metadata()
            .map_err(|source| PathError::GeneralIOError {
                path: inner.to_owned(),
                source,
            })?;
        if metadata.is_dir() {
            Ok(SubDirectoryResult::SubDirectory(Self(inner)))
        } else {
            Ok(SubDirectoryResult::IsNotDirectory)
        }
    }

    pub fn join_file_name(
        &self,
        segment: &NormalPathSegment,
    ) -> Result<CanonicalChildFileBuf, PathError> {
        let path = self.0.join(&segment.0);
        check_is_file_or_missing(&path, "")?; // TODO: Better error here
        Ok(CanonicalChildFileBuf(path))
    }
}

// File is not absolute, do not contain . or .. and may not exist
#[derive(Debug, Hash, PartialEq, Eq)]
pub struct RelativePathBuf(PathBuf);

impl RelativePathBuf {
    pub fn new(path: &Path) -> Option<Self> {
        if path.is_absolute() {
            return None;
        }

        if path.as_os_str().is_empty() {
            return None;
        }

        if !path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        {
            return None;
        }

        return Some(RelativePathBuf(path.to_owned()));
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

impl From<NormalPathSegmentBuf> for RelativePathBuf {
    fn from(item: NormalPathSegmentBuf) -> Self {
        Self(item.0.into())
    }
}

impl From<&std::fs::DirEntry> for RelativePathBuf {
    fn from(item: &std::fs::DirEntry) -> Self {
        let segment: NormalPathSegmentBuf = item.into();
        segment.into()
    }
}

// File may not exist, but is absolute and normalized.
// Its parent is canonical.
// If it exists, it must be a file
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

        check_is_file_or_missing(&full_path, description)?;
        Ok(Self(full_path))
    }

    pub fn parent(&self) -> CanonicalDirBuf {
        // This should never fail, because the invariants of CanonicalDirBuf
        // tests that the parent exists and is a canonical directory
        CanonicalDirBuf(self.0.parent().unwrap().to_owned())
    }
}

fn check_is_file_or_missing(path: &Path, description: &str) -> Result<(), PathError> {
    match path.metadata() {
        Ok(md) => {
            if md.is_file() {
                Ok(())
            } else {
                Err(PathError::IsNotFileOrMissing {
                    description: description.to_owned(),
                    path: path.to_owned(),
                })
            }
        }
        Err(inner) => match inner.kind() {
            ErrorKind::NotFound => Ok(()),
            _ => Err(PathError::GeneralIOError {
                path: path.to_owned(),
                source: inner,
            }),
        },
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
    IsNotFileOrMissing { description: String, path: PathBuf },
}
