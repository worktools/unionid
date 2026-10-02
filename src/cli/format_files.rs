//! File-oriented formatter. Validate the entire batch before changing any file.
use super::*;

pub fn run(paths: Vec<PathBuf>, check: bool, write: bool) -> Result<(), String> {
    if paths.is_empty() {
        if write {
            return Err("E_CONFIG: fmt --write requires at least one file".into());
        }
        return format_source(&read_source(io::stdin().lock())?, check);
    }
    if paths.len() > 1 && !check && !write {
        return Err("E_CONFIG: multiple files require --check or --write".into());
    }
    let mut files = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for path in paths {
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("inspect '{}': {error}", path.display()))?;
        if write && (!metadata.is_file() || metadata.file_type().is_symlink()) {
            return Err(format!(
                "E_CONFIG: fmt --write requires regular files, not links or directories: '{}'",
                path.display()
            ));
        }
        let canonical = std::fs::canonicalize(&path)
            .map_err(|error| format!("resolve '{}': {error}", path.display()))?;
        if !seen.insert(canonical) {
            continue;
        }
        let source = read_source(
            std::fs::File::open(&path)
                .map_err(|error| format!("open '{}': {error}", path.display()))?,
        )?;
        let formatted = crate::format_source(&source)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        files.push((path, source, formatted, metadata.permissions()));
    }
    if check {
        let changed = files
            .iter()
            .filter(|(_, source, formatted, _)| source != formatted)
            .map(|(path, _, _, _)| path.display().to_string())
            .collect::<Vec<_>>();
        if !changed.is_empty() {
            return Err(format!(
                "input is not canonically formatted:\n{}",
                changed.join("\n")
            ));
        }
    } else if write {
        for (path, source, formatted, permissions) in files {
            if source == formatted {
                continue;
            }
            replace(&path, &source, &formatted, permissions)?;
        }
    } else {
        io::stdout()
            .write_all(files[0].2.as_bytes())
            .map_err(|error| format!("write formatted source: {error}"))?;
    }
    Ok(())
}

fn replace(
    path: &Path,
    original: &str,
    formatted: &str,
    permissions: std::fs::Permissions,
) -> Result<(), String> {
    if permissions.readonly() {
        return Err(format!(
            "refusing to replace read-only file '{}'",
            path.display()
        ));
    }
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|error| format!("create formatter temporary name: {error}"))?;
    let temporary = path.with_file_name(format!(
        ".unionid-fmt-{:032x}.tmp",
        u128::from_le_bytes(nonce)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options
        .open(&temporary)
        .map_err(|error| format!("create temporary file for '{}': {error}", path.display()))?;
    let result = (|| -> io::Result<()> {
        output.write_all(formatted.as_bytes())?;
        output.set_permissions(permissions)?;
        output.sync_all()?;
        drop(output);
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || std::fs::read_to_string(path)? != original
        {
            return Err(io::Error::other(
                "source changed during formatting; retry after saving edits",
            ));
        }
        std::fs::rename(&temporary, path)
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("replace '{}': {error}", path.display()));
    }
    Ok(())
}
