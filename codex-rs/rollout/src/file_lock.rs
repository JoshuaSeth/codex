use std::io;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExclusiveLockAttempt {
    Acquired,
    Contended,
}

/// Tries to claim the advisory exclusive lock shared by rollout writers and
/// maintenance jobs. The caller must retain `file` for as long as it owns the
/// lock; closing the file releases the lock.
#[cfg(unix)]
pub(crate) fn try_lock_exclusive<T>(file: &T, path: &Path) -> io::Result<ExclusiveLockAttempt>
where
    T: std::os::fd::AsRawFd + ?Sized,
{
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(ExclusiveLockAttempt::Acquired);
    }

    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        return Ok(ExclusiveLockAttempt::Contended);
    }

    Err(io::Error::new(
        error.kind(),
        format!(
            "failed to acquire exclusive rollout lock for {}: {error}",
            path.display()
        ),
    ))
}

#[cfg(not(unix))]
pub(crate) fn try_lock_exclusive<T>(_file: &T, _path: &Path) -> io::Result<ExclusiveLockAttempt>
where
    T: ?Sized,
{
    Ok(ExclusiveLockAttempt::Acquired)
}
