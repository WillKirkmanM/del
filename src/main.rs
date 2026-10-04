use clap::Parser;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Preview or delete files selected by Windows-style filename patterns."
)]
struct Args {
    #[arg(
        required = true,
        value_name = "PATH_OR_PATTERN",
        help = "File paths or filename patterns containing * and ?; quote patterns to prevent shell expansion"
    )]
    selections: Vec<OsString>,

    #[arg(
        short,
        long,
        help = "Search subdirectories beneath each selection's directory"
    )]
    recursive: bool,

    #[arg(
        long,
        conflicts_with = "apply",
        help = "Preview selected files (the default)"
    )]
    preview: bool,

    #[arg(
        long,
        help = "Delete selected files; read-only files retain their attributes"
    )]
    apply: bool,
}

#[derive(Debug)]
struct SelectedFile {
    path: PathBuf,
    read_only: bool,
}

fn filename_matches(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let name: Vec<char> = name.to_lowercase().chars().collect();
    let mut row = vec![false; name.len() + 1];
    row[0] = true;
    for token in pattern {
        let mut next = vec![false; name.len() + 1];
        for index in 0..=name.len() {
            match token {
                '*' => next[index] = row[index] || (index > 0 && next[index - 1]),
                '?' => {
                    next[index] = (row[index] && (index == name.len() || name[index] == '.'))
                        || (index > 0 && name[index - 1] != '.' && row[index - 1]);
                }
                '.' => {
                    next[index] = (index > 0 && name[index - 1] == '.' && row[index - 1])
                        || (index == name.len() && row[index]);
                }
                _ => next[index] = index > 0 && name[index - 1] == token && row[index - 1],
            }
        }
        row = next;
    }
    row[name.len()]
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn checked_directory(path: &Path) -> io::Result<PathBuf> {
    let absolute = absolute_path(path)?;
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            return Err(io::Error::other(format!(
                "directory symlinks are not searched: {}",
                current.display()
            )));
        }
        if !metadata.is_dir() {
            return Err(io::Error::other(format!(
                "not a directory: {}",
                current.display()
            )));
        }
    }
    fs::canonicalize(absolute)
}

fn insert_file(path: PathBuf, files: &mut BTreeMap<PathBuf, SelectedFile>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("not a regular file"));
    }
    let canonical = fs::canonicalize(&path)?;
    files.entry(canonical.clone()).or_insert(SelectedFile {
        path: canonical,
        read_only: metadata.permissions().readonly(),
    });
    Ok(())
}

fn search(
    root: &Path,
    pattern: &str,
    recursive: bool,
    wildcard: bool,
    files: &mut BTreeMap<PathBuf, SelectedFile>,
) -> io::Result<usize> {
    let mut pending = vec![root.to_owned()];
    let mut matched = 0;
    while let Some(directory) = pending.pop() {
        checked_directory(&directory)?;
        let mut entries = fs::read_dir(&directory)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            let is_match = name.to_str().is_some_and(|name| {
                if wildcard {
                    filename_matches(pattern, name)
                } else {
                    name.to_lowercase() == pattern.to_lowercase()
                }
            });
            if file_type.is_symlink() {
                if is_match && !wildcard {
                    return Err(io::Error::other(format!(
                        "symlinks are not selected: {}",
                        entry.path().display()
                    )));
                }
                continue;
            }
            if file_type.is_dir() {
                if is_match && !wildcard {
                    return Err(io::Error::other(format!(
                        "directories cannot be deleted: {}",
                        entry.path().display()
                    )));
                }
                if recursive {
                    pending.push(entry.path());
                }
            } else if file_type.is_file() && is_match {
                insert_file(entry.path(), files)?;
                matched += 1;
            }
        }
    }
    Ok(matched)
}

fn select(
    selection: &Path,
    recursive: bool,
    files: &mut BTreeMap<PathBuf, SelectedFile>,
) -> io::Result<()> {
    let filename = selection
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a file path or filename pattern",
            )
        })?;
    if filename.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty filename",
        ));
    }
    let parent = selection
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if parent.components().any(|component| {
        matches!(component, Component::Normal(name) if name.to_string_lossy().contains(['*', '?']))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "wildcards are only allowed in the filename",
        ));
    }
    let root = checked_directory(parent)?;
    let wildcard = filename.contains(['*', '?']);
    if search(&root, filename, recursive, wildcard, files)? == 0 {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no matching files"));
    }
    Ok(())
}

fn delete_file(file: &SelectedFile) -> io::Result<()> {
    let parent = file
        .path
        .parent()
        .ok_or_else(|| io::Error::other("file has no parent directory"))?;
    checked_directory(parent)?;
    let metadata = fs::symlink_metadata(&file.path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("no longer a regular file"));
    }
    if metadata.permissions().readonly() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "read-only file; attributes unchanged",
        ));
    }
    fs::remove_file(&file.path)
}

fn run(args: Args) -> io::Result<bool> {
    let mut files = BTreeMap::new();
    let mut selection_failed = 0;
    for selection in &args.selections {
        if let Err(error) = select(Path::new(selection), args.recursive, &mut files) {
            eprintln!("error: {}: {error}", Path::new(selection).display());
            selection_failed += 1;
        }
    }
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut deleted = 0;
    let mut failed = 0;
    for file in files.values() {
        let label = if file.read_only { " [read-only]" } else { "" };
        writeln!(output, "{}{}", file.path.display(), label)?;
    }
    output.flush()?;
    if selection_failed > 0 && args.apply {
        eprintln!("error: deletion cancelled because selection failed");
    } else if args.apply {
        for file in files.values() {
            match delete_file(file) {
                Ok(()) => deleted += 1,
                Err(error) => {
                    failed += 1;
                    eprintln!("error: {}: {error}", file.path.display());
                }
            }
        }
    }
    let mode = if args.apply { "Apply" } else { "Preview" };
    writeln!(
        output,
        "{mode}: selected {}, deleted {deleted}, failed {failed}",
        files.len()
    )?;
    if selection_failed > 0 {
        writeln!(output, "Selection errors: {selection_failed}")?;
    }
    Ok(selection_failed == 0 && failed == 0)
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join(format!(
                    "del-test-{}-{}",
                    std::process::id(),
                    NEXT_ID.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn windows_patterns_ignore_case_and_allow_missing_extensions() {
        assert!(filename_matches("*.TXT", "Notes.txt"));
        assert!(filename_matches("*.*", "README"));
        assert!(filename_matches("report.*", "REPORT"));
        assert!(filename_matches("file?.txt", "file1.txt"));
        assert!(filename_matches("file?.txt", "file.txt"));
        assert!(!filename_matches("file?.txt", "file12.txt"));
        assert!(!filename_matches("*.txt", "notes.csv"));
        assert!(filename_matches("*a*b", "AAab"));
        assert!(!filename_matches("a*b", "abx"));
    }

    #[test]
    fn parser_rejects_conflicting_modes_and_missing_paths() {
        assert!(Args::try_parse_from(["del", "--preview", "--apply", "file.txt"]).is_err());
        assert!(Args::try_parse_from(["del"]).is_err());
        assert!(!Args::try_parse_from(["del", "file.txt"]).unwrap().apply);
    }

    #[test]
    fn recursive_selection_is_sorted_deduplicated_and_preserves_directories() {
        let directory = TestDirectory::new();
        fs::create_dir(directory.0.join("child")).unwrap();
        fs::create_dir(directory.0.join("folder.txt")).unwrap();
        fs::write(directory.0.join("B.txt"), "b").unwrap();
        fs::write(directory.0.join("a.TXT"), "a").unwrap();
        fs::write(directory.0.join("child/c.txt"), "c").unwrap();
        let mut files = BTreeMap::new();
        select(&directory.0.join("*.txt"), false, &mut files).unwrap();
        assert_eq!(files.len(), 2);
        select(&directory.0.join("*.TXT"), true, &mut files).unwrap();
        select(&directory.0.join("A.txt"), false, &mut files).unwrap();
        assert_eq!(files.len(), 3);
        let mut literal_files = BTreeMap::new();
        select(&directory.0.join("C.TXT"), true, &mut literal_files).unwrap();
        assert_eq!(literal_files.len(), 1);
        let paths: Vec<_> = files.keys().collect();
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(select(&directory.0.join("folder.txt"), false, &mut files).is_err());
        assert!(select(&directory.0.join("missing.txt"), false, &mut files).is_err());
        assert!(select(&directory.0.join("*/a.txt"), false, &mut files).is_err());
        for file in files.values() {
            delete_file(file).unwrap();
        }
        assert!(directory.0.join("folder.txt").is_dir());
    }

    #[test]
    fn read_only_files_are_reported_and_preserved() {
        let directory = TestDirectory::new();
        let path = directory.0.join("protected.txt");
        fs::write(&path, "retain").unwrap();
        let original = fs::metadata(&path).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        fs::set_permissions(&path, permissions).unwrap();
        let mut files = BTreeMap::new();
        select(&path, false, &mut files).unwrap();
        let file = files.values().next().unwrap();
        assert!(file.read_only);
        assert!(delete_file(file).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "retain");
        assert!(fs::metadata(&path).unwrap().permissions().readonly());
        fs::set_permissions(path, original).unwrap();
    }
}
