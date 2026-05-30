// The code in this module repsents file-system level knowledge about paths using newtypes.
// There is plenty of opportunities for TOCTOU issues, but we don't want to anticipiate those
// in this program.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use std::{
    borrow::Borrow,
    ffi::{OsStr, OsString},
    fs::DirEntry,
    io::ErrorKind,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};

use crate::{AppError, UserError};

// This type represents a single, normal path segment. That is, an element of the path, which:
// * Does not contain the root, or a Windows drive segment
// * Does not contain a path separator
// * Is not . or ..
// * Is not the empty string
#[repr(transparent)]
pub struct NormalPathSegment(OsStr);

impl NormalPathSegment {
    pub fn new(path: &Path) -> Option<&Self> {
        // Check path contains exactly one segment, which is Normal
        let mut components = path.components();
        match components.next() {
            None => return None,
            Some(std::path::Component::Normal(_)) => (),
            Some(_) => return None,
        }
        if components.next().is_some() {
            return None;
        }
        unsafe { Some(Self::from_os_str(path.as_os_str())) }
    }

    // Safety: The invariants above must be upheld
    unsafe fn from_os_str(s: &OsStr) -> &Self {
        // SAFETY: `MyOsStr` has the same layout as `OsStr`
        unsafe { &*(s as *const OsStr as *const Self) }
    }
}

// Owned version of NormalPathSegment, with same invariants
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct NormalPathSegmentBuf(OsString);

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

// This has the same invariants as NormalPathSegment, but also guarantees
// that the segment is UTF8.
#[derive(Serialize, Clone, Debug, Hash, PartialEq, Eq)]
pub struct NormalUTF8Segment(String);

impl NormalUTF8Segment {
    pub fn new(value: String) -> Option<Self> {
        NormalPathSegment::new(Path::new(&value))?;
        Some(Self(value))
    }

    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn from_timestamp() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let s = format!(
            ".sequencer-sync-write-probe-{}-{timestamp}",
            std::process::id()
        );
        // Safety: Since timestamp is ASCII digits, this is alphanumerical,
        // except the leading dot and dashes, and so upholds the invariant.
        Self(s)
    }

    pub fn as_normal(&self) -> &NormalPathSegment {
        // Safety: Self's invariants are a superset of NormalPathSegment
        unsafe { NormalPathSegment::from_os_str(OsStr::from_bytes(self.0.as_bytes())) }
    }
}

impl<'de> Deserialize<'de> for NormalUTF8Segment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| {
                 D::Error::custom("expected a normal UTF-8 path segment: non-empty, not '.', not '..', and no path separators")
            })
    }
}

// What can happen when a DirEntry of reading a dir and looking for normal UTF8
// subdirectories
pub enum DirEntrySubdirCases {
    // Unexpected IO Error
    IOError(std::io::Error),
    // Found a normal UTF8 subdir
    UTF8SubDir {
        full_path: CanonicalDirBuf,
        last_segment: NormalUTF8Segment,
    },
    IsSymlink,
    IsNotDir,
    NotUTF8,
}

// Get last segment of this entry
pub fn utf8_subdir(entry: &DirEntry) -> DirEntrySubdirCases {
    let md = match entry.metadata() {
        Ok(md) => md,
        Err(e) => return DirEntrySubdirCases::IOError(e),
    };

    if md.is_symlink() {
        return DirEntrySubdirCases::IsSymlink;
    }
    if !md.is_dir() {
        return DirEntrySubdirCases::IsNotDir;
    }
    let file_name = entry.file_name();
    NormalPathSegment::new(Path::new(&file_name)).unwrap_or_else(|| {
        panic!(
            "DirEntry segment ought to be a Normal segment, got {}",
            file_name.display()
        )
    });
    // This is not entirely safe, because this presupposes the user created the
    // DirEntry from an already canonical path.
    let canonical = CanonicalDirBuf(entry.path());
    match file_name.into_string() {
        Ok(s) => DirEntrySubdirCases::UTF8SubDir {
            full_path: canonical,
            last_segment: NormalUTF8Segment(s),
        },
        Err(_) => DirEntrySubdirCases::NotUTF8,
    }
}

impl From<NormalUTF8Segment> for NormalPathSegmentBuf {
    fn from(value: NormalUTF8Segment) -> Self {
        NormalPathSegmentBuf(value.into_inner().into())
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

// File exists, is a directory, and is absolute and normalized
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct CanonicalDirBuf(PathBuf);

impl AsRef<Path> for CanonicalDirBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

// When building a CanonicalDirBuf from a CanonicalDirBuf and a NormalPathSegment,
// you can get these errors (and also IOError, not covered here)
pub enum SubDirectoryResult {
    IsNotDirectory,
    IsSymlink,
    SubDirectory(CanonicalDirBuf),
}

impl SubDirectoryResult {
    pub fn expect_normal_dir(self, s: &str) -> CanonicalDirBuf {
        match self {
            Self::IsNotDirectory | Self::IsSymlink => panic!("{}", s),
            Self::SubDirectory(x) => x,
        }
    }
}

impl CanonicalDirBuf {
    pub unsafe fn new_unchecked(p: PathBuf) -> Self {
        Self(p)
    }

    pub fn create_if_not_exist(&self, subdir: &NormalPathSegment) -> Result<Self> {
        let path = self.as_ref().join(subdir.as_ref());
        match std::fs::create_dir(&path) {
            Ok(()) => Ok(CanonicalDirBuf(path)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                match self.try_from_existing_subdirectory(subdir)? {
                    SubDirectoryResult::SubDirectory(s) => Ok(s),
                    SubDirectoryResult::IsSymlink => {
                        bail!(
                            "Program logic assumed the following was a directory, but it was a symlink {:?}.\n\
                            This is probably an internal program error",
                            path
                        );
                    }
                    SubDirectoryResult::IsNotDirectory => {
                        bail!(
                            "Program logic assumed the following was a directory, but it was another type of file: {:?}.\n\
                            This is probably an internal program error",
                            path
                        );
                    }
                }
            }
            Err(source) => bail!("{source}"),
        }
    }

    pub fn create_subdir(&self, subdir: &NormalPathSegment) -> Result<Self> {
        let path = self.as_ref().join(subdir.as_ref());
        match std::fs::create_dir(&path) {
            Ok(()) => {
                // Guaranteed to work, since we just created the directory
                // and the normal segment is already normalized
                Ok(CanonicalDirBuf(path))
            }
            Err(source) => bail!("Failure to create sub-directory at {:?}, {source}", path),
        }
    }

    pub fn from_absolute(path: &Path, description: &str) -> Result<Self, AppError> {
        check_absolute(path, description)?;
        match path.canonicalize() {
            Ok(path) => {
                if path.is_dir() {
                    Ok(Self(path))
                } else {
                    Err(UserError::NotADirectory {
                        description: description.to_owned(),
                        path: path.to_owned(),
                    }
                    .into())
                }
            }
            Err(source) => match source.kind() {
                ErrorKind::NotFound => Err(UserError::NotFound {
                    description: description.to_owned(),
                    path: path.to_owned(),
                }
                .into()),
                _ => Err(AppError::Internal(anyhow::Error::from(source))),
            },
        }
    }

    pub fn try_from_existing_subdirectory(
        &self,
        relative: &NormalPathSegment,
    ) -> Result<SubDirectoryResult> {
        let inner = self.0.join(Path::new(&relative.0));
        if inner.is_symlink() {
            return Ok(SubDirectoryResult::IsSymlink);
        };
        let metadata = inner.metadata().with_context(|| {
            format!(
                "Failure to get metadata of supposedly existing sub-directory at {:?}",
                inner
            )
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
        description: &str,
    ) -> Result<CanonicalChildFileBuf, AppError> {
        let path = self.0.join(&segment.0);
        check_is_file_or_missing(&path, description)?;
        Ok(CanonicalChildFileBuf(path))
    }
}

// Is not absolute, and therefore cannot be considered canonical
// (i.e. it makes no sense to query the file system to normalize it)
// but it should be lexographically normalized (i.e. no segments with)
// . or .. ie all segments should be Component::Normal
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

        Some(RelativePathBuf(path.to_owned()))
    }

    // Returns None if there is only one segment in Self
    pub fn parent(&self) -> Option<Self> {
        // Safety: Is None if self is empty, or ends in a non-Normal segment.
        // However, we know from the invariants of Self that cannot happen
        let s = self.0.parent().unwrap().as_os_str();
        if s.is_empty() {
            None
        } else {
            Some(Self(Path::new(s).to_owned()))
        }
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

// This represents a child of a CanonicalDirBuf, which is either an existing
// file (not a dir), or which does not exist.
// The last segment must be a Normal segment.
// This is used to represent e.g. a log file, which may not have been created yet.
#[derive(Debug)]
pub struct CanonicalChildFileBuf(PathBuf);

impl AsRef<Path> for CanonicalChildFileBuf {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl CanonicalChildFileBuf {
    pub fn from_absolute(path: &Path, description: &str) -> Result<Self, AppError> {
        check_absolute(path, description)?;

        // This can only fail if paths ends in ..
        let file_name = path
            .file_name()
            .ok_or_else(|| UserError::PathEndsInParent {
                description: description.to_owned(),
                path: path.to_owned(),
            })?;

        // Check it has a parent
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| UserError::PathHasNoParent {
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

fn check_is_file_or_missing(path: &Path, description: &str) -> Result<(), AppError> {
    match path.metadata() {
        Ok(md) => {
            if md.is_symlink() {
                return Err(UserError::IsSymlinkNotRegularFile {
                    description: description.to_owned(),
                    path: path.to_owned(),
                }
                .into());
            }

            if md.is_file() {
                Ok(())
            } else {
                Err(UserError::IsNotFileOrMissing {
                    description: description.to_owned(),
                    path: path.to_owned(),
                }
                .into())
            }
        }
        Err(inner) => match inner.kind() {
            ErrorKind::NotFound => Ok(()),
            _ => Err(AppError::internal_from(inner)),
        },
    }
}

fn check_absolute(path: &Path, description: &str) -> Result<(), UserError> {
    if !path.is_absolute() {
        return Err(UserError::PathNotAbsolute {
            description: description.to_owned(),
            path: path.to_owned(),
        });
    }
    Ok(())
}
