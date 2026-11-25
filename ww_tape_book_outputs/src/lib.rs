//! What the book shows of a program's output, under `docs/generated/`.
//!
//! A test calls [check] with what a reader sees when running a program; it fails when the file holds something
//! else. With `WW_TAPE_BOOK_OUTPUTS` set to `write` (`just book-outputs`), [check] writes the file instead. Such tests
//! carry `book_output` in their name, so that `just book-outputs` finds them.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

/// The variable that makes [check] write its file, when set to `write`.
const WRITE_ENV: &str = "WW_TAPE_BOOK_OUTPUTS";

/// Where the outputs are, from the repository root.
const GENERATED_DIR: &str = "docs/generated";

/// The longest name of an output, in bytes.
const NAME_BYTES_MAX: usize = 128; // spirit-level: allow(exact-units-quantity) compared with str::len, a byte count

/// Why [check] failed.
#[derive(Debug)]
pub enum Error {
    /// The name is not a relative path of lowercase letters, digits, `_`, `-` and `.`.
    Name(String),
    /// The file differs from the output, or is missing.
    Stale {
        /// The file, from the repository root.
        file: PathBuf,
        /// The first line that differs, counted from 1.
        line: usize,
    },
    /// Reading or writing the file failed.
    Io(PathBuf, std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => write!(
                f,
                "`{name}` is not an output name: lowercase letters, digits, `_`, `-` and `.`, in `/`-separated \
                 segments, at most {NAME_BYTES_MAX} bytes"
            ),
            Self::Stale { file, line } => write!(
                f,
                "{} is stale from line {line}: run `just book-outputs`",
                file.display()
            ),
            Self::Io(file, error) => write!(f, "{}: {error}", file.display()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(_, error) => Some(error),
            Self::Name(_) | Self::Stale { .. } => None,
        }
    }
}

/// Whether `name` is a relative path of plain segments.
fn valid_name(name: &str) -> bool {
    name.len() <= NAME_BYTES_MAX
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
                })
        })
}

/// The repository root: the nearest directory above this crate that holds `zensical.toml`.
fn repository_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|dir| dir.join("zensical.toml").is_file())
        .map(Path::to_path_buf)
}

/// Checks `docs/generated/<name>` under `root` against `text`, or writes it when `write` is set.
fn check_in(root: &Path, name: &str, text: &str, write: bool) -> Result<(), Error> {
    if !valid_name(name) {
        return Err(Error::Name(name.to_owned()));
    }
    let relative = Path::new(GENERATED_DIR).join(name);
    let file = root.join(&relative);
    if write {
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|e| Error::Io(relative.clone(), e))?;
        }
        return fs::write(&file, text).map_err(|e| Error::Io(relative, e));
    }
    let on_disk = match fs::read_to_string(&file) {
        Ok(on_disk) => on_disk,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(Error::Io(relative, error)),
    };
    if on_disk == text {
        return Ok(());
    }
    let line = text
        .lines()
        .zip(on_disk.lines())
        .take_while(|(ours, theirs)| ours == theirs)
        .count()
        .saturating_add(1);
    Err(Error::Stale {
        file: relative,
        line,
    })
}

/// Checks `docs/generated/<name>` against `text`, or writes it when `WW_TAPE_BOOK_OUTPUTS` is `write`.
///
/// # Panics
///
/// When the file is stale or missing, cannot be read or written, or `name` is not a plain relative path: this is
/// the calling test's assertion.
#[expect(clippy::panic, reason = "the harness's assertion: a stale output fails the calling test")]
pub fn check(name: &str, text: &str) {
    let write = std::env::var(WRITE_ENV).is_ok_and(|value| value == "write");
    let Some(root) = repository_root() else {
        // spirit-level: allow(panic-in-library) the harness's assertion: the crate sits in the book's repository
        panic!("ww_tape_book_outputs sits outside the repository of the book");
    };
    if let Err(error) = check_in(&root, name, text, write) {
        // spirit-level: allow(panic-in-library) the harness's assertion: a stale output fails the calling test
        panic!("{error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ww_tape_book_outputs_{name}_{}",
            std::process::id()
        ));
        _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_then_passes_then_fails_when_stale() {
        let root = scratch("cycle");
        let missing = check_in(&root, "ex/out.txt", "a\nb\n", false).unwrap_err();
        assert!(matches!(missing, Error::Stale { line: 1, .. }), "{missing}");
        check_in(&root, "ex/out.txt", "a\nb\n", true).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("docs/generated/ex/out.txt")).unwrap(),
            "a\nb\n"
        );
        check_in(&root, "ex/out.txt", "a\nb\n", false).unwrap();
        let stale = check_in(&root, "ex/out.txt", "a\nc\n", false).unwrap_err();
        assert_eq!(
            stale.to_string(),
            "docs/generated/ex/out.txt is stale from line 2: run `just book-outputs`"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn names_stay_inside_the_directory() {
        let root = scratch("names");
        for name in [
            "../out.txt",
            "/abs.txt",
            "a//b.txt",
            "Up.txt",
            "a/./b.txt",
            "",
        ] {
            assert!(
                matches!(check_in(&root, name, "", true), Err(Error::Name(_))),
                "{name}"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
