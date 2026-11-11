//! Git LFS: fetching the large files of an archive with `git lfs`, and
//! reading them back for the file view, the raw file and the ZIP or tar.gz
//! download. The objects live in `<repo>.git/lfs/objects/<aa>/<bb>/<oid>`,
//! where `git lfs` keeps them in a bare repository.

use std::path::{Path, PathBuf};

use super::command::{Access, Git, GitError};

/// A pointer file is always smaller than this.
pub const POINTER_MAX_BYTES: usize = 1024;
const VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1";

/// `-c` options that make `git archive` write the content of LFS files.
/// A missing object is never downloaded during a request: the empty file
/// URL fails at once, and the pointer file stays in the archive.
pub const ARCHIVE_CONFIG: [&str; 8] = [
    "-c",
    "filter.lfs.process=git-lfs filter-process",
    "-c",
    "filter.lfs.required=false",
    "-c",
    "lfs.url=file:///nonexistent/kehribar-offline",
    "-c",
    "lfs.skipdownloaderrors=true",
];

/// The content of a pointer file: the SHA-256 and the size of the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pointer {
    pub oid: String,
    pub size: u64,
}

/// Reads a pointer file; `None` for any other content.
pub fn parse_pointer(data: &[u8]) -> Option<Pointer> {
    if data.len() >= POINTER_MAX_BYTES {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    if lines.next()? != VERSION_LINE {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("oid sha256:") {
            oid = Some(value);
        } else if let Some(value) = line.strip_prefix("size ") {
            size = value.parse().ok();
        }
    }
    Some(Pointer {
        oid: oid.filter(|oid| is_oid(oid))?.to_owned(),
        size: size?,
    })
}

/// 64 lowercase hex digits, so an object id is always a safe file name.
fn is_oid(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn objects_dir(dir: &Path) -> PathBuf {
    dir.join("lfs").join("objects")
}

/// Where `git lfs` keeps the object `oid` of the repository at `dir`.
/// `oid` must have passed [`parse_pointer`].
fn object_path(dir: &Path, oid: &str) -> PathBuf {
    objects_dir(dir).join(&oid[..2]).join(&oid[2..4]).join(oid)
}

/// The file of the object `pointer` names, when the archive holds it whole.
pub async fn local_object(dir: &Path, pointer: &Pointer) -> std::io::Result<Option<PathBuf>> {
    let path = object_path(dir, &pointer.oid);
    match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() && meta.len() == pointer.size => Ok(Some(path)),
        Ok(_) => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Downloads every LFS object that any ref of the archive points to.
pub async fn fetch(git: &Git, dir: &Path, access: Option<&Access>) -> Result<(), GitError> {
    git.run(Some(dir), ["lfs", "fetch", "--all", "origin"], access)
        .await
        .map(|_| ())
}

/// Every object the archive at `dir` holds, as object id and file.
pub async fn local_objects(dir: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let root = objects_dir(dir);
    tokio::task::spawn_blocking(move || {
        let mut found = Vec::new();
        collect_objects(&root, 0, &mut found)?;
        found.sort();
        Ok(found)
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Walks the two folder levels `<aa>/<bb>` and keeps the object files.
/// Temporary and partial downloads sit elsewhere, under `lfs/tmp`.
fn collect_objects(
    dir: &Path,
    depth: usize,
    found: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    for entry in entries {
        collect_entry(&entry?, depth, found)?;
    }
    Ok(())
}

/// A folder above the objects is walked; at the object level, a file named
/// like an object id is kept.
fn collect_entry(
    entry: &std::fs::DirEntry,
    depth: usize,
    found: &mut Vec<(String, PathBuf)>,
) -> std::io::Result<()> {
    let file_type = entry.file_type()?;
    if depth < 2 {
        if file_type.is_dir() {
            collect_objects(&entry.path(), depth + 1, found)?;
        }
        return Ok(());
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if is_oid(&name) && file_type.is_file() {
        found.push((name, entry.path()));
    }
    Ok(())
}

/// Bytes the LFS objects of the archive at `dir` take.
pub async fn objects_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for (_, path) in local_objects(dir).await? {
        total += tokio::fs::metadata(&path).await?.len();
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "429e45f22916bccc31fc8f9c2b7de8de3198ce4ff14c7d572fe7497de6a9b2f0";

    fn pointer_text(oid: &str) -> String {
        format!("{VERSION_LINE}\noid sha256:{oid}\nsize 2000\n")
    }

    #[test]
    fn pointers_are_read_and_anything_else_is_not() {
        assert_eq!(
            parse_pointer(pointer_text(OID).as_bytes()),
            Some(Pointer {
                oid: OID.into(),
                size: 2000
            })
        );
        // An object id that could leave the objects folder is refused.
        assert_eq!(
            parse_pointer(pointer_text("../../etc/passwd").as_bytes()),
            None
        );
        assert_eq!(
            parse_pointer(pointer_text(&OID.to_uppercase()).as_bytes()),
            None
        );
        assert_eq!(parse_pointer(b"hello\n"), None);
        let no_size = format!("{VERSION_LINE}\noid sha256:{OID}\n");
        assert_eq!(parse_pointer(no_size.as_bytes()), None);
        let long = format!("{}{}", pointer_text(OID), "x".repeat(POINTER_MAX_BYTES));
        assert_eq!(parse_pointer(long.as_bytes()), None);
    }

    #[tokio::test]
    async fn local_objects_are_found_only_whole_and_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let pointer = parse_pointer(pointer_text(OID).as_bytes()).unwrap();
        assert_eq!(local_object(dir.path(), &pointer).await.unwrap(), None);
        assert!(local_objects(dir.path()).await.unwrap().is_empty());

        let path = object_path(dir.path(), OID);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![7u8; 1999]).unwrap();
        // A file of the wrong size is not the object.
        assert_eq!(local_object(dir.path(), &pointer).await.unwrap(), None);
        std::fs::write(&path, vec![7u8; 2000]).unwrap();
        assert_eq!(
            local_object(dir.path(), &pointer).await.unwrap(),
            Some(path.clone())
        );
        std::fs::write(path.with_file_name("not-an-oid"), b"x").unwrap();
        assert_eq!(
            local_objects(dir.path()).await.unwrap(),
            [(OID.to_owned(), path)]
        );
        assert_eq!(objects_size(dir.path()).await.unwrap(), 2000);
    }
}
