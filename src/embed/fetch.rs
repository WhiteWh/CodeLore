//! Getting the pinned model files: download them from Hugging Face into the
//! user cache, or verify a directory an offline install already holds.
//!
//! Every file is pinned by exact size and SHA-256 (each model's list in
//! [`super::models`]); a download goes to a temporary file beside its
//! destination, is hashed while it streams, and only a match is renamed into
//! place. A network failure, a short read, a wrong size or a wrong digest
//! leaves whatever was there.
use super::models::EmbeddingModel;
use super::{Error, ModelSource, PinnedFile};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const CHUNK: usize = 64 * 1024;
/// A download may not run past the pinned size by more than this before it is
/// refused; the final size must match exactly.
const SLACK: u64 = 1024;

/// Fetches every pinned file of `model` into the cache (or verifies `dir` for
/// an offline install). Returns the model directory.
pub fn fetch(model: EmbeddingModel, source: ModelSource<'_>, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<PathBuf, Error> {
    let dir = match source {
        ModelSource::Cache => super::model_dir(model),
        ModelSource::Local(path) => path.to_path_buf(),
    };
    match source {
        ModelSource::Local(_) => verify_all(model, &dir)?,
        ModelSource::Cache => {
            std::fs::create_dir_all(&dir)
                .map_err(|error| Error::Fetch(format!("cannot create {}: {error}", dir.display())))?;
            for file in model.files() {
                ensure(model, &dir, file, progress)?;
            }
        }
    }
    Ok(dir)
}

/// Every pinned file's exact size and SHA-256.
pub fn verify_all(model: EmbeddingModel, dir: &Path) -> Result<(), Error> {
    for file in model.files() {
        verify_file(dir, file)?;
    }
    Ok(())
}

/// A file's SHA-256 read in chunks; the file's size is checked first.
fn hash(path: &Path) -> Result<String, Error> {
    let mut reader = std::fs::File::open(path)
        .map_err(|error| Error::Model(format!("cannot read {}: {error}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::Model(format!("cannot read {}: {error}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_file(dir: &Path, file: &PinnedFile) -> Result<(), Error> {
    let path = dir.join(file.name);
    let metadata = std::fs::metadata(&path)
        .map_err(|error| Error::Model(format!("{}: {error}", path.display())))?;
    if !metadata.is_file() || metadata.len() != file.bytes {
        return Err(Error::Model(format!(
            "{} is {} bytes, the pinned file is {} bytes",
            path.display(),
            metadata.len(),
            file.bytes
        )));
    }
    let digest = hash(&path)?;
    if digest != file.sha256 {
        return Err(Error::Model(format!(
            "{} has SHA-256 {digest}, the pinned file is {}",
            path.display(),
            file.sha256
        )));
    }
    Ok(())
}

/// One file: kept when it already matches, downloaded and verified otherwise.
fn ensure(model: EmbeddingModel, dir: &Path, file: &PinnedFile, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<(), Error> {
    if verify_file(dir, file).is_ok() {
        progress(file.name, file.bytes, file.bytes);
        return Ok(());
    }
    download(model, file, &dir.join(file.name), progress)
}

/// Streams `file` from Hugging Face at the model's revision and renames it into
/// place only when its size and digest match the pin.
fn download(model: EmbeddingModel, file: &PinnedFile, path: &Path, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<(), Error> {
    let url = format!(
        "https://huggingface.co/{}/resolve/{}/{}",
        model.repository(),
        model.revision(),
        file.name
    );
    let mut response = ureq::get(&url)
        .call()
        .map_err(|error| Error::Fetch(format!("{url}: {error}")))?;
    if !response.status().is_success() {
        return Err(Error::Fetch(format!("{url}: HTTP {}", response.status())));
    }
    let mut temporary = path.to_path_buf();
    temporary.set_file_name(format!(".{}.{}.part", file.name, std::process::id()));
    let mut output = std::fs::File::create(&temporary)
        .map_err(|error| Error::Fetch(format!("cannot write {}: {error}", temporary.display())))?;
    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut reader = response.body_mut().as_reader();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| Error::Fetch(format!("{url}: {error}")))?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > file.bytes + SLACK {
            let _ = std::fs::remove_file(&temporary);
            return Err(Error::Fetch(format!("{url}: the file is larger than its pinned {} bytes", file.bytes)));
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| Error::Fetch(format!("cannot write {}: {error}", temporary.display())))?;
        progress(file.name, written, file.bytes);
    }
    output
        .flush()
        .map_err(|error| Error::Fetch(format!("cannot write {}: {error}", temporary.display())))?;
    drop(output);
    if written != file.bytes {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::Fetch(format!("{url}: got {written} bytes, the pinned file is {} bytes", file.bytes)));
    }
    let digest = format!("{:x}", hasher.finalize());
    if digest != file.sha256 {
        let _ = std::fs::remove_file(&temporary);
        return Err(Error::Fetch(format!(
            "{url}: SHA-256 {digest} does not match the pinned {}; the file was not installed",
            file.sha256
        )));
    }
    replace(&temporary, path)
        .map_err(|error| Error::Fetch(format!("cannot install {}: {error}", path.display())))
}

/// Renames a verified download over its destination.
fn replace(temporary: &Path, path: &Path) -> std::io::Result<()> {
    match std::fs::rename(temporary, path) {
        Ok(()) => Ok(()),
        Err(first) => {
            let _ = std::fs::remove_file(path);
            std::fs::rename(temporary, path).map_err(|_| first)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lore-fetch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_file_is_verified_by_its_exact_size_and_sha256() {
        let dir = scratch("verify");
        let file = PinnedFile { name: "small.bin", bytes: 5, sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824" };
        std::fs::write(dir.join(file.name), b"hello").unwrap();
        assert!(verify_file(&dir, &file).is_ok());
        std::fs::write(dir.join(file.name), b"hellO").unwrap();
        assert!(verify_file(&dir, &file).is_err());
        std::fs::write(dir.join(file.name), b"hel").unwrap();
        assert!(verify_file(&dir, &file).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_offline_directory_is_verified_before_it_is_used() {
        let dir = scratch("offline");
        let error = fetch(EmbeddingModel::E5Base, ModelSource::Local(&dir), &mut |_, _, _| {}).unwrap_err();
        assert!(matches!(error, Error::Model(_)), "{error}");
        std::fs::write(dir.join(EmbeddingModel::E5Base.files()[0].name), vec![0u8; 16]).unwrap();
        let error = fetch(EmbeddingModel::E5Base, ModelSource::Local(&dir), &mut |_, _, _| {}).unwrap_err();
        assert!(matches!(error, Error::Model(_)), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
