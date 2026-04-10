// Shared test helper functions for cmd modules.

use nix::errno::Errno;

/// Verify that `result` is `Err(expected_errno)`.
#[allow(clippy::needless_pass_by_value)]
pub fn expect_errno<T>(result: nix::Result<T>, expected: Errno, context: &str) -> nix::Result<()> {
    match result {
        Err(e) if e == expected => Ok(()),
        Err(e) => {
            tracing::error!(context, expected = %expected, got = %e, "wrong errno");
            Err(Errno::EIO)
        }
        Ok(_) => {
            tracing::error!(context, expected = %expected, "expected error, got Ok");
            Err(Errno::EIO)
        }
    }
}

/// Verify that `result` is `Err` with one of the expected errno values.
#[allow(clippy::needless_pass_by_value)]
pub fn expect_errno_one_of<T>(
    result: nix::Result<T>,
    expected: &[Errno],
    context: &str,
) -> nix::Result<()> {
    match result {
        Err(e) if expected.contains(&e) => Ok(()),
        Err(e) => {
            tracing::error!(context, expected = ?expected, got = %e, "wrong errno");
            Err(Errno::EIO)
        }
        Ok(_) => {
            tracing::error!(context, expected = ?expected, "expected error, got Ok");
            Err(Errno::EIO)
        }
    }
}
