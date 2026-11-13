//! Atomic replacement of small durable records.

use std::path::Path;

use tempfile::{NamedTempFile, PersistError};

/// Publishes `temporary` over `path`.
///
/// Short-lived Windows readers and virus scanners can briefly deny
/// replacement (access denied, sharing or lock violation). Windows keeps the
/// complete temporary record and retries the atomic publish with backoff for
/// about 600 ms. Other platforms replace the file once.
pub(crate) fn persist_replacing(
    temporary: NamedTempFile,
    path: &Path,
) -> Result<std::fs::File, PersistError> {
    #[cfg(windows)]
    {
        let mut temporary = temporary;
        let mut retry_delay_ms = 10;
        loop {
            match temporary.persist(path) {
                Ok(file) => return Ok(file),
                Err(error)
                    if matches!(error.error.raw_os_error(), Some(5 | 32 | 33))
                        && retry_delay_ms <= 320 =>
                {
                    temporary = error.file;
                    std::thread::sleep(std::time::Duration::from_millis(retry_delay_ms));
                    retry_delay_ms *= 2;
                }
                Err(error) => return Err(error),
            }
        }
    }
    #[cfg(not(windows))]
    temporary.persist(path)
}
