//! PreprocessorFs implementation using std::fs

use std::borrow::Cow;
use std::path::PathBuf;

use glsl::syntax::Path;
use thiserror::Error;

use super::PreprocessorFs;

/// Implementation of [super::PreprocessorFs] for [std::fs]
#[derive(Default, Debug, Clone, Copy)]
pub struct StdPreprocessorFs<'i> {
    include: &'i [PathBuf],
}

impl<'i> StdPreprocessorFs<'i> {
    /// Create a new StdPreprocessorFs instance with no system include path
    pub fn new() -> Self {
        Self { include: &[] }
    }

    /// Create a new StdPreprocessorFs instance with the given include path
    ///
    /// # Parameters
    ///
    /// * `include`: list of paths to include directories to check for absolute includes
    pub fn with_include_path(include: &'i [PathBuf]) -> Self {
        Self { include }
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
    UnresolvedInclude(glsl::syntax::Path),
    /// Parse error
    #[error("parse error: {0}")]
    ParseError(#[from] glsl::parser::ParseError),
}

impl PreprocessorFs for StdPreprocessorFs<'_> {
    type Error = StdPreprocessorFsError;

    fn read(&self, path: &std::path::Path) -> Result<Cow<str>, Self::Error> {
        Ok(Cow::Owned(std::fs::read_to_string(path)?))
    }

    fn canonicalize(&self, path: &std::path::Path) -> Result<PathBuf, Self::Error> {
        Ok(std::fs::canonicalize(path)?)
    }

    fn resolve(&self, base_path: &PathBuf, path: &Path) -> Result<PathBuf, Self::Error> {
        match &path {
            Path::Absolute(abs_path) => {
                let path_buf = PathBuf::from(abs_path);

                self.include
                    .iter()
                    .find_map(|dir| std::fs::canonicalize(dir.join(&path_buf)).ok())
            }
            Path::Relative(path) => {
                let path = PathBuf::from(path);

                std::iter::once(base_path)
                    .chain(self.include.iter())
                    .find_map(|dir| std::fs::canonicalize(dir.join(&path)).ok())
            }
        }
        .ok_or_else(|| Self::Error::UnresolvedInclude(path.clone()))
    }
}
