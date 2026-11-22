use crate::PlatformError;
use std::path::{Path, PathBuf};

const SHA256: &str = "3256173f3f8bffd7df12227c68adf68025edb1832273a9530688a7bb1ed8edec";

#[cfg(feature = "embedded-ffmpeg")]
mod embedded {
    use super::SHA256;
    use crate::PlatformError;
    use sha2::{Digest, Sha256};
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    const PROGRAM: &[u8] = include_bytes!("../../media/ffmpeg-win64.gz");
    const STAMP_FILE: &str = "ffmpeg.verified";
    static CHECKED: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub(super) fn executable(dir: &Path) -> Result<PathBuf, PlatformError> {
        let dir = dir.join("media").join(SHA256);
        let file = dir.join("ffmpeg.exe");
        let mut checked = CHECKED
            .lock()
            .map_err(|e| PlatformError::System(e.to_string()))?;
        if checked.as_ref() != Some(&file) || !file.is_file() {
            install(&dir, &file)
                .map_err(|e| PlatformError::System(format!("准备视频编码器失败：{e}")))?;
            *checked = Some(file.clone());
        }
        Ok(file)
    }

    pub(super) fn verify_cold(dir: &Path) -> Result<PathBuf, PlatformError> {
        let media_dir = dir.join("media").join(SHA256);
        let file = media_dir.join("ffmpeg.exe");
        install(&media_dir, &file)
            .map_err(|e| PlatformError::System(format!("准备视频编码器失败：{e}")))?;
        Ok(file)
    }

    fn expected_stamp(meta: &std::fs::Metadata) -> Option<String> {
        let modified_ns = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos();
        Some(format!("{}:{}:{}", meta.len(), modified_ns, SHA256))
    }

    fn write_stamp(dir: &Path, path: &Path) {
        if let Ok(meta) = std::fs::metadata(path)
            && let Some(stamp) = expected_stamp(&meta)
        {
            let stamp_path = dir.join(STAMP_FILE);
            let tmp = dir.join(format!("{STAMP_FILE}.{}.tmp", fastrand::u64(..)));
            if std::fs::write(&tmp, stamp.as_bytes()).is_ok() {
                let _ = std::fs::rename(&tmp, &stamp_path);
            } else {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }

    fn is_verified_fast(dir: &Path, path: &Path) -> io::Result<bool> {
        let meta = match std::fs::metadata(path) {
            Ok(m) if m.is_file() => m,
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if let Some(expected) = expected_stamp(&meta)
            && let Ok(saved) = std::fs::read_to_string(dir.join(STAMP_FILE))
            && saved.trim() == expected
        {
            return Ok(true);
        }
        if matches_hash(path)? {
            write_stamp(dir, path);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn matches_hash(path: &Path) -> io::Result<bool> {
        let mut input = match File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut hash = Sha256::new();
        let mut buf = vec![0u8; 512 * 1024];
        loop {
            let n = io::Read::read(&mut input, &mut buf)?;
            if n == 0 {
                break;
            }
            hash.update(&buf[..n]);
        }
        Ok(format!("{:x}", hash.finalize()) == SHA256)
    }

    fn install(dir: &Path, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let license = dir.join("LICENSE-FFmpeg.txt");
        if !license.exists() {
            std::fs::write(license, include_bytes!("../../media/LICENSE-FFmpeg.txt"))?;
        }
        let build = dir.join("FFmpeg-build.txt");
        if !build.exists() {
            std::fs::write(build, include_bytes!("../../media/FFmpeg-build.txt"))?;
        }
        if is_verified_fast(dir, path)? {
            return Ok(());
        }
        let temp = dir.join(format!("{}.{}.tmp", std::process::id(), fastrand::u64(..)));
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = (|| {
            io::copy(&mut flate2::read::GzDecoder::new(PROGRAM), &mut output)?;
            output.sync_all()?;
            drop(output);
            if !matches_hash(&temp)? {
                return Err(io::Error::other("编码器校验失败"));
            }
            match std::fs::rename(&temp, path) {
                Err(_) if matches_hash(path)? => std::fs::remove_file(&temp)?,
                other => other?,
            }
            write_stamp(dir, path);
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(not(feature = "embedded-ffmpeg"))]
mod external {
    use super::SHA256;
    use crate::PlatformError;
    use std::path::{Path, PathBuf};

    pub(super) fn executable(dir: &Path) -> Result<PathBuf, PlatformError> {
        let cached = dir.join("media").join(SHA256).join("ffmpeg.exe");
        if cached.is_file() {
            return Ok(cached);
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            let sibling = parent.join("ffmpeg.exe");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
        if let Some(paths) = std::env::var_os("PATH") {
            for entry in std::env::split_paths(&paths) {
                let candidate = entry.join("ffmpeg.exe");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(PlatformError::Unsupported(
            "当前构建未内置视频编码器，且在同目录与 PATH 中均未找到 ffmpeg.exe".to_string(),
        ))
    }
}

pub(super) fn executable(dir: &Path) -> Result<PathBuf, PlatformError> {
    #[cfg(feature = "embedded-ffmpeg")]
    {
        embedded::executable(dir)
    }
    #[cfg(not(feature = "embedded-ffmpeg"))]
    {
        external::executable(dir)
    }
}

#[doc(hidden)]
pub fn verify_embedded_encoder_ready(dir: &Path) -> Result<PathBuf, PlatformError> {
    #[cfg(feature = "embedded-ffmpeg")]
    {
        embedded::verify_cold(dir)
    }
    #[cfg(not(feature = "embedded-ffmpeg"))]
    {
        external::executable(dir)
    }
}
