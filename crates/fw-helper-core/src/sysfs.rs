use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A filesystem root for hardware access.
///
/// All reads and writes are relative to `root`, which makes the entire layer
/// testable against a fixture directory instead of real hardware.
#[derive(Debug, Clone)]
pub struct Sysfs {
    root: PathBuf,
}

impl Default for Sysfs {
    fn default() -> Self {
        Self::new("/")
    }
}

impl Sysfs {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }

    /// Resolve a relative sysfs path. Leading slashes are tolerated so callers may
    /// write either `/sys/...` or `sys/...`.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.join(rel.trim_start_matches('/'))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).exists()
    }

    pub fn read_string(&self, rel: &str) -> io::Result<String> {
        Ok(fs::read_to_string(self.path(rel))?.trim().to_string())
    }

    pub fn read_u64(&self, rel: &str) -> io::Result<u64> {
        let raw = self.read_string(rel)?;
        raw.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{rel}: cannot parse {raw:?} as u64: {e}"),
            )
        })
    }

    pub fn read_i64(&self, rel: &str) -> io::Result<i64> {
        let raw = self.read_string(rel)?;
        raw.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{rel}: cannot parse {raw:?} as i64: {e}"),
            )
        })
    }

    pub fn write_string(&self, rel: &str, value: &str) -> io::Result<()> {
        fs::write(self.path(rel), value)
    }

    /// Locate a hwmon node by its `name` file.
    ///
    /// hwmon indices are assigned in probe order and are **not** stable across boots —
    /// on the reference machine `cros_ec` was `hwmon11`, but nothing guarantees that.
    /// Returns a path relative to the root, suitable for the other methods here.
    pub fn find_hwmon(&self, name: &str) -> Option<String> {
        let dir = self.path("sys/class/hwmon");
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let base = entry.file_name().to_str()?.to_string();
            let rel = format!("sys/class/hwmon/{base}");
            if self.read_string(&format!("{rel}/name")).ok().as_deref() == Some(name) {
                return Some(rel);
            }
        }
        None
    }
}
