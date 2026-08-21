use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::Write,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

const ACTIVATION_FILE: &str = "show-window.request";

pub struct SingleInstanceGuard {
    _lock_file: File,
}

impl SingleInstanceGuard {
    pub fn try_acquire(state_directory: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(state_directory).context("failed to create state directory")?;
        let lock_path = state_directory.join("instance.lock");
        let mut lock_file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open {}", lock_path.display()))?;

        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(error)) => {
                return Err(error).context("failed to lock WinKeeper instance");
            }
        }

        let _ = fs::remove_file(state_directory.join(ACTIVATION_FILE));
        lock_file.set_len(0)?;
        writeln!(lock_file, "{}", std::process::id())?;
        Ok(Some(Self {
            _lock_file: lock_file,
        }))
    }

    pub fn request_activation(state_directory: &Path) -> Result<()> {
        fs::create_dir_all(state_directory).context("failed to create state directory")?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        fs::write(
            state_directory.join(ACTIVATION_FILE),
            format!("{} {nonce}\n", std::process::id()),
        )
        .context("failed to request activation from the running WinKeeper instance")
    }

    pub fn take_activation_request(state_directory: &Path) -> Result<bool> {
        match fs::remove_file(state_directory.join(ACTIVATION_FILE)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error).context("failed to consume WinKeeper activation request"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_only_one_guard_per_state_directory() {
        let directory = std::env::temp_dir().join(format!(
            "win-keeper-single-instance-test-{}",
            std::process::id()
        ));
        let first = SingleInstanceGuard::try_acquire(&directory)
            .unwrap()
            .expect("first instance should acquire the lock");
        assert!(
            SingleInstanceGuard::try_acquire(&directory)
                .unwrap()
                .is_none()
        );
        drop(first);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            if SingleInstanceGuard::try_acquire(&directory)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "instance lock was not released"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn activation_request_is_consumed_once() {
        let directory =
            std::env::temp_dir().join(format!("win-keeper-activation-test-{}", std::process::id()));
        SingleInstanceGuard::request_activation(&directory).unwrap();
        assert!(SingleInstanceGuard::take_activation_request(&directory).unwrap());
        assert!(!SingleInstanceGuard::take_activation_request(&directory).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }
}
