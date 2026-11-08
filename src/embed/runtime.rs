//! DirectML.dll for the `directml` build: fetched once into the shared models
//! folder (`<models>/runtimes/directml-ort-1.31.0/DirectML.dll`), pinned by
//! the size and SHA-256 of the archive ONNX Runtime's Rust binding publishes
//! for this ONNX Runtime (pyke's `x86_64-pc-windows-msvc+directml` package,
//! the same one the build links against), and of the DLL inside it. Loaded by
//! its full path before the first DirectML session: ONNX Runtime then finds
//! it already in the process instead of Windows' older system copy.
use crate::embed::Error;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;

const ARCHIVE_URL: &str = "https://cdn.pyke.io/0/pyke:ort-rs/ms@1.31.0/x86_64-pc-windows-msvc+directml.tar.lzma2";
const ARCHIVE_BYTES: u64 = 32_457_673;
const ARCHIVE_SHA256: &str = "f99aaef23f12986abb05924c944d8433521263ad756b462df762a5a982c98405";
const DLL_NAME: &str = "DirectML.dll";
const DLL_BYTES: u64 = 18_527_776;
const DLL_SHA256: &str = "9c9e6d822561c6c41b90e6994b3e8857cf1d66dbfb1e0c4c799c7c89b4e92da1";

/// Where the pinned DirectML.dll lives.
pub fn directml_path() -> PathBuf {
    crate::embed::models_root().join("runtimes").join("directml-ort-1.31.0").join(DLL_NAME)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The pinned DLL is in place (size checked; the digest was checked when it
/// was written).
pub fn directml_installed() -> bool {
    std::fs::metadata(directml_path()).is_ok_and(|meta| meta.len() == DLL_BYTES)
}

/// Fetches and unpacks the pinned DirectML.dll. The archive is a raw LZMA2
/// stream of a tar file; only the DLL is taken from it.
pub fn fetch_directml() -> Result<PathBuf, Error> {
    let target = directml_path();
    if directml_installed() {
        return Ok(target);
    }
    let mut response = ureq::get(ARCHIVE_URL).call().map_err(|error| Error::Fetch(format!("{ARCHIVE_URL}: {error}")))?;
    let mut archive = Vec::with_capacity(ARCHIVE_BYTES as usize);
    response
        .body_mut()
        .with_config()
        .limit(ARCHIVE_BYTES + 1)
        .reader()
        .read_to_end(&mut archive)
        .map_err(|error| Error::Fetch(format!("{ARCHIVE_URL}: {error}")))?;
    if archive.len() as u64 != ARCHIVE_BYTES || sha256_hex(&archive) != ARCHIVE_SHA256 {
        return Err(Error::Fetch(format!("{ARCHIVE_URL}: the archive is not the pinned one ({} bytes)", archive.len())));
    }
    let dll = extract(&archive, DLL_NAME)?;
    if dll.len() as u64 != DLL_BYTES || sha256_hex(&dll) != DLL_SHA256 {
        return Err(Error::Fetch(format!("{DLL_NAME} in the archive is not the pinned one")));
    }
    let parent = target.parent().map(PathBuf::from).unwrap_or_default();
    std::fs::create_dir_all(&parent).map_err(|error| Error::Fetch(format!("cannot create {}: {error}", parent.display())))?;
    let temporary = parent.join(format!(".{DLL_NAME}.{}.part", std::process::id()));
    std::fs::write(&temporary, &dll).map_err(|error| Error::Fetch(format!("cannot write {}: {error}", temporary.display())))?;
    std::fs::rename(&temporary, &target).or_else(|error| if directml_installed() { Ok(()) } else { Err(error) }).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        Error::Fetch(format!("cannot place {}: {error}", target.display()))
    })?;
    Ok(target)
}

/// One file's bytes out of the tar inside the LZMA2 stream.
fn extract(archive: &[u8], wanted: &str) -> Result<Vec<u8>, Error> {
    let bad = |what: &str| Error::Fetch(format!("the DirectML archive is unreadable: {what}"));
    let mut tar = lzma_rust2::Lzma2Reader::new(archive, 1 << 26, None);
    let mut header = [0u8; 512];
    loop {
        tar.read_exact(&mut header).map_err(|_| bad("a header is cut short"))?;
        if header.iter().all(|b| *b == 0) {
            return Err(bad(&format!("{wanted} is not in it")));
        }
        let field = |from: usize, len: usize| -> String {
            let bytes = &header[from..from + len];
            let end = bytes.iter().position(|b| *b == 0).unwrap_or(len);
            String::from_utf8_lossy(&bytes[..end]).trim().to_owned()
        };
        let size = u64::from_str_radix(&field(124, 12), 8).map_err(|_| bad("a size is not octal"))?;
        let name = field(0, 100);
        let padded = size.next_multiple_of(512);
        if name == wanted && header[156] == b'0' {
            let mut data = vec![0u8; size as usize];
            tar.read_exact(&mut data).map_err(|_| bad("the file is cut short"))?;
            return Ok(data);
        }
        std::io::copy(&mut (&mut tar).take(padded), &mut std::io::sink()).map_err(|_| bad("an entry is cut short"))?;
    }
}

/// DirectML.dll in this process: fetched if missing, loaded by its path.
pub fn load_directml() -> Result<(), Error> {
    static LOADED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    LOADED
        .get_or_init(|| {
            let path = fetch_directml().map_err(|error| error.to_string())?;
            if crate::embed::system::load_library(path.as_os_str()) {
                Ok(())
            } else {
                Err(format!("{} could not be loaded", path.display()))
            }
        })
        .clone()
        .map_err(Error::NoEmbeddings)
}
