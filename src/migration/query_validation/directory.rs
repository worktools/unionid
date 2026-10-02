//! Load a bounded, immutable set before opening a database for migration.
use std::fs;
use std::io::Read;
use std::path::Path;

use super::{MAX_QUERY_FILES, MAX_QUERY_REPORT_BYTES, MAX_QUERY_SOURCE_BYTES, MigrationQuery};
use crate::error::{Error, Result};

pub const MAX_QUERY_DIRECTORY_DEPTH: usize = 32;
/// Includes directories, so empty directory trees cannot bypass the file budget.
pub const MAX_QUERY_DIRECTORY_ENTRIES: usize = 4_096;

/// Recursively load `.unid` and legacy `.uid` files in relative-path order.
/// Reject symlinks and non-source entries instead of silently skipping them.
/// The caller owns warnings and presentation; this loader never writes output.
pub fn load_query_directory(root: impl AsRef<Path>) -> Result<Vec<MigrationQuery>> {
    load(root.as_ref(), false)
}

pub(crate) fn load_optional_query_directory(root: &Path) -> Result<Vec<MigrationQuery>> {
    load(root, true)
}

fn load(root: &Path, allow_empty: bool) -> Result<Vec<MigrationQuery>> {
    let metadata = fs::symlink_metadata(root).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::new(
            "E_MIGRATION",
            "query directory must be a real directory, not a symlink",
        ));
    }
    let mut loader = Loader {
        queries: Vec::new(),
        entries: 0,
        source_bytes: 0,
        path_bytes: 0,
        discovery: allow_empty,
    };
    loader.visit(root, root, 0)?;
    if loader.queries.is_empty() && !allow_empty {
        return Err(Error::new(
            "E_MIGRATION",
            "query directory contains no query files",
        ));
    }
    loader.queries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(loader.queries)
}

fn io_error(error: std::io::Error) -> Error {
    Error::new("E_IO", format!("cannot load query directory: {error}"))
}

struct Loader {
    queries: Vec<MigrationQuery>,
    entries: usize,
    source_bytes: usize,
    path_bytes: usize,
    discovery: bool,
}

impl Loader {
    fn visit(&mut self, root: &Path, directory: &Path, depth: usize) -> Result<()> {
        if depth > MAX_QUERY_DIRECTORY_DEPTH {
            return Err(Error::new("E_LIMIT", "query directory depth exceeds 32"));
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(directory).map_err(io_error)? {
            self.entries += 1;
            if self.entries > MAX_QUERY_DIRECTORY_ENTRIES {
                return Err(Error::new(
                    "E_LIMIT",
                    "query directory entry budget exceeded",
                ));
            }
            entries.push(entry.map_err(io_error)?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let kind = entry.file_type().map_err(io_error)?;
            if kind.is_dir() {
                self.visit(root, &path, depth + 1)?;
                continue;
            }
            if !kind.is_file() {
                return Err(Error::new(
                    "E_MIGRATION",
                    "query directory contains a symlink or non-regular entry",
                ));
            }
            if !matches!(
                path.extension().and_then(|x| x.to_str()),
                Some("unid" | "uid")
            ) {
                if self.discovery {
                    continue;
                }
                return Err(Error::new(
                    "E_MIGRATION",
                    "query directory accepts only .unid or legacy .uid files",
                ));
            }
            if self.queries.len() == MAX_QUERY_FILES {
                return Err(Error::new(
                    "E_LIMIT",
                    "query directory file budget exceeded",
                ));
            }
            let relative = path.strip_prefix(root).expect("entry stays below root");
            let relative = relative
                .to_str()
                .ok_or_else(|| Error::new("E_MIGRATION", "query paths must be UTF-8"))?;
            // Normalize separators without flattening directories or conflating basenames.
            let relative = if std::path::MAIN_SEPARATOR == '\\' {
                relative.replace('\\', "/")
            } else {
                relative.to_owned()
            };
            self.path_bytes += relative.len();
            if self.path_bytes > MAX_QUERY_REPORT_BYTES {
                return Err(Error::new("E_LIMIT", "query path report budget exceeded"));
            }
            let mut bytes = Vec::new();
            fs::File::open(&path)
                .map_err(io_error)?
                .take((crate::syntax::MAX_SOURCE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map_err(io_error)?;
            self.source_bytes += bytes.len();
            if bytes.len() > crate::syntax::MAX_SOURCE_BYTES
                || self.source_bytes > MAX_QUERY_SOURCE_BYTES
            {
                return Err(Error::new("E_LIMIT", "query source byte budget exceeded"));
            }
            let source = String::from_utf8(bytes)
                .map_err(|_| Error::new("E_INPUT", "query source must be UTF-8"))?;
            self.queries.push(MigrationQuery {
                path: relative,
                source,
            });
        }
        Ok(())
    }
}
