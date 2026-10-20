//! The app's log file, `<SILO_HOME>/app.log`, besides stderr (lost when
//! Finder launches the app). Capped: past [`CAP`] it starts over, the
//! previous one kept as `app.log.old`.

use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const CAP: u64 = 1024 * 1024;

pub struct AppLog {
    path: PathBuf,
    cap: u64,
    /// The open file and its length.
    file: Mutex<(File, u64)>,
}

impl AppLog {
    /// # Errors
    /// The log cannot be opened.
    pub fn open(home: &Path) -> io::Result<Self> {
        Self::with_cap(home.join("app.log"), CAP)
    }

    fn with_cap(path: PathBuf, cap: u64) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = append(&path)?;
        let len = file.metadata()?.len();
        Ok(Self { path, cap, file: Mutex::new((file, len)) })
    }
}

fn append(path: &Path) -> io::Result<File> {
    File::options().create(true).append(true).open(path)
}

impl Write for &AppLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if file.1 > 0 && file.1 + buf.len() as u64 > self.cap {
            std::fs::rename(&self.path, self.path.with_extension("log.old"))?;
            *file = (append(&self.path)?, 0);
        }
        let written = file.0.write(buf)?;
        file.1 += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.lock().unwrap_or_else(std::sync::PoisonError::into_inner).0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_over_past_its_cap() {
        let dir = std::env::temp_dir().join(format!("silo-app-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("app.log");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "earlier\n").unwrap();
        let log = AppLog::with_cap(path.clone(), 16).unwrap();
        (&log).write_all(b"one\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "earlier\none\n", "appended");
        (&log).write_all(b"two two two\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two two two\n");
        assert_eq!(std::fs::read_to_string(dir.join("app.log.old")).unwrap(), "earlier\none\n");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
