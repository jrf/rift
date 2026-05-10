use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5MB

pub struct LogSystem {
    inner: Mutex<LogInner>,
}

struct LogInner {
    file: Option<File>,
    current_size: u64,
    path: PathBuf,
}

impl LogSystem {
    pub fn new() -> Self {
        LogSystem {
            inner: Mutex::new(LogInner {
                file: None,
                current_size: 0,
                path: PathBuf::new(),
            }),
        }
    }

    pub fn init(&self, path: &Path) -> std::io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let current_size = file.metadata()?.len();

        let mut inner = self.inner.lock().unwrap();
        inner.file = Some(file);
        inner.current_size = current_size;
        inner.path = path.to_path_buf();

        // Set file permissions to 0o640
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o640));
        }

        Ok(())
    }

    pub fn log(&self, level: log::Level, target: &str, message: &str) {
        let mut inner = self.inner.lock().unwrap();

        if inner.file.is_none() {
            eprintln!("[{}] ({}): {}", level, target, message);
            return;
        }

        if inner.current_size >= MAX_SIZE {
            if let Err(e) = rotate(&mut inner) {
                eprintln!("log rotation failed: {}", e);
            }
            if inner.file.is_none() {
                return;
            }
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();

        let line = format!("[{}] [{}] ({}): {}\n", now, level, target, message);
        let len = line.len() as u64;

        if let Some(ref mut f) = inner.file {
            if f.write_all(line.as_bytes()).is_ok() {
                inner.current_size += len;
            }
        }
    }
}

fn rotate(inner: &mut LogInner) -> std::io::Result<()> {
    if let Some(f) = inner.file.take() {
        drop(f);
    }

    let old_path = format!("{}.old", inner.path.display());
    match fs::rename(&inner.path, &old_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&inner.path)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&inner.path, fs::Permissions::from_mode(0o640));
    }

    inner.file = Some(file);
    inner.current_size = 0;
    Ok(())
}

/// Implements `log::Log` so the standard `log` macros route through our file logger.
impl log::Log for LogSystem {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let msg = format!("{}", record.args());
            LogSystem::log(self, record.level(), record.target(), &msg);
        }
    }

    fn flush(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(ref f) = inner.file {
            // File opened in append mode; sync to ensure durability
            let _ = f.sync_all();
        }
    }
}
