use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};

use crate::{AppError, AppResult, paths::atomic_write};

pub(crate) const INSTALL_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const SERVER_LOG_MAX_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct BoundedLog {
    file: File,
    length: u64,
    limit: u64,
}

impl BoundedLog {
    pub(crate) fn open(path: &Path, limit: u64) -> AppResult<Self> {
        if limit == 0 {
            return Err(AppError::new("logLimitInvalid"));
        }
        trim_log_tail(path, limit)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(path)
            .map_err(|error| AppError::io_path(path, &error))?;
        let length = file
            .metadata()
            .map_err(|error| AppError::io_path(path, &error))?
            .len();
        Ok(Self {
            file,
            length,
            limit,
        })
    }

    pub(crate) fn write_line(&mut self, line: &str) -> AppResult<()> {
        let maximum_payload = usize::try_from(self.limit.saturating_sub(1))
            .map_err(|_| AppError::new("logLimitInvalid"))?;
        let bytes = line.as_bytes();
        let bytes = if bytes.len() > maximum_payload {
            // Keep the bounded tail aligned to a UTF-8 character boundary so
            // the log file never contains truncated multi-byte sequences.
            let mut start = bytes.len() - maximum_payload;
            while start < bytes.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            &bytes[start..]
        } else {
            bytes
        };
        let required = bytes.len() as u64 + 1;
        if self.length.saturating_add(required) > self.limit {
            self.file.set_len(0)?;
            self.length = 0;
        }
        self.file.write_all(bytes)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.length = self.length.saturating_add(required);
        Ok(())
    }
}

pub(crate) fn trim_log_tail(path: &Path, limit: u64) -> AppResult<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(AppError::io_path(path, &error)),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::new("logPathUnsafe").value("path", path.display()));
    }
    if metadata.len() <= limit {
        return Ok(());
    }
    let keep = usize::try_from(limit).map_err(|_| AppError::new("logLimitInvalid"))?;
    let offset = i64::try_from(limit).map_err(|_| AppError::new("logLimitInvalid"))?;
    let mut file = File::open(path).map_err(|error| AppError::io_path(path, &error))?;
    file.seek(SeekFrom::End(-offset))
        .map_err(|error| AppError::io_path(path, &error))?;
    let mut tail = vec![0_u8; keep];
    file.read_exact(&mut tail)
        .map_err(|error| AppError::io_path(path, &error))?;
    atomic_write(path, &tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_log_never_exceeds_its_limit() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("service.log");
        let mut log = BoundedLog::open(&path, 16).unwrap();

        log.write_line("1234567890").unwrap();
        log.write_line("abcdefghij").unwrap();

        assert!(fs::metadata(&path).unwrap().len() <= 16);
        assert_eq!(fs::read_to_string(path).unwrap(), "abcdefghij\n");
    }

    #[test]
    fn bounded_log_truncation_keeps_valid_utf8() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("service.log");
        let mut log = BoundedLog::open(&path, 16).unwrap();

        // Long multi-byte line: the bounded tail must stay valid UTF-8 even
        // when the byte cut would land inside a character.
        log.write_line(&"日志行".repeat(16)).unwrap();

        let content = fs::read_to_string(path).unwrap();
        assert!(content.len() <= 16);
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn oversized_existing_log_keeps_only_a_bounded_tail() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("install.log");
        fs::write(&path, b"0123456789abcdef").unwrap();

        trim_log_tail(&path, 6).unwrap();

        assert_eq!(fs::read(path).unwrap(), b"abcdef");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_log_never_follows_a_link_outside_the_application_home() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside.log");
        let link = temp.path().join("server.log");
        fs::write(&outside, b"user-data").unwrap();
        symlink(&outside, &link).unwrap();

        let error = BoundedLog::open(&link, 16).err().unwrap();

        assert_eq!(error.code, "logPathUnsafe");
        assert_eq!(fs::read(outside).unwrap(), b"user-data");
    }
}
