//! PreprocessorFs implementation using std::fs

use std::borrow::Cow;
use std::path::{self, PathBuf};

use glsl_lang::ast::{Path, PathData};
use thiserror::Error;

use super::PreprocessorFs;

/// Implementation of [super::PreprocessorFs] for [std::fs]
#[derive(Default, Debug, Clone)]
pub struct StdPreprocessorFs {
    include: Vec<PathBuf>,
}

impl StdPreprocessorFs {
    /// Create a new StdPreprocessorFs instance with no system include path
    pub fn new() -> Self {
        Self { include: vec![] }
    }

    /// Create a new StdPreprocessorFs instance with the given include path
    ///
    /// # Parameters
    ///
    /// * `include`: list of paths to include directories to check for absolute includes
    pub fn with_include_path(include: &[PathBuf]) -> Self {
        Self {
            include: include.to_vec(),
        }
    }
}

/// std::fs resolver error
#[derive(Debug, Error)]
pub enum StdPreprocessorFsError {
    /// I/O error
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Unresolved include directive
    #[error("unresolved include: {0:?}")]
    UnresolvedInclude(Path),
    /// Parse error
    #[error("parse error: {0}")]
    ParseError(#[from] glsl_lang::parse::ParseError),
}

impl PartialEq for StdPreprocessorFsError {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Self::Io(a) => match other {
                Self::Io(b) => a.kind() == b.kind(),
                _ => false,
            },
            Self::UnresolvedInclude(a) => match other {
                Self::UnresolvedInclude(b) => a == b,
                _ => false,
            },
            Self::ParseError(a) => match other {
                Self::ParseError(b) => a == b,
                _ => false,
            },
        }
    }
}

impl PreprocessorFs for StdPreprocessorFs {
    type Error = StdPreprocessorFsError;

    fn read(&self, path: &path::Path) -> Result<Cow<str>, Self::Error> {
        Ok(Cow::Owned(std::fs::read_to_string(path)?))
    }

    fn canonicalize(&self, path: &path::Path) -> Result<PathBuf, Self::Error> {
        Ok(std::fs::canonicalize(path)?)
    }

    fn resolve(&self, base_path: &path::Path, path: &Path) -> Result<PathBuf, Self::Error> {
        match &**path {
            PathData::Absolute(abs_path) => {
                let path_buf = PathBuf::from(abs_path);

                self.include
                    .iter()
                    .find_map(|dir| std::fs::canonicalize(dir.join(&path_buf)).ok())
            }
            PathData::Relative(path) => {
                let path = PathBuf::from(path);

                std::iter::once(base_path)
                    .chain(self.include.iter().map(|p| p.as_path()))
                    .find_map(|dir| std::fs::canonicalize(dir.join(&path)).ok())
            }
        }
        .ok_or_else(|| Self::Error::UnresolvedInclude(path.clone()))
    }
}
