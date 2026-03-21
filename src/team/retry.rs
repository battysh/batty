//! Retry helpers for transient failures with configurable exponential backoff.

use std::fmt::Debug;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::warn;

use super::git_cmd::GitError;

/// Classifies whether an error is safe to retry.
pub trait Retryable {
    fn is_transient(&self) -> bool;
}

impl Retryable for GitError {
    fn is_transient(&self) -> bool {
        self.is_transient()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub base_delay_ms: u64,
    pub max_delay_ms: u64,
    pub jitter: bool,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 100,
            max_delay_ms: 5_000,
            jitter: true,
        }
    }
}

impl RetryConfig {
    pub fn no_retry() -> Self {
        Self {
            max_retries: 0,
            ..Self::default()
        }
    }

    pub fn fast() -> Self {
        Self {
            max_retries: 2,
            base_delay_ms: 50,
            max_delay_ms: 500,
            jitter: true,
        }
    }

    pub fn conservative() -> Self {
        Self {
            max_retries: 5,
            base_delay_ms: 200,
            max_delay_ms: 10_000,
            jitter: true,
        }
    }
}

/// TODO: implement `Retryable` for typed `BoardError` once that module lands in
/// this branch.
pub fn retry_sync<T, E, F>(config: &RetryConfig, operation: F) -> Result<T, E>
where
    E: Retryable + Debug,
    F: Fn() -> Result<T, E>,
{
    retry_sync_with_sleep(config, operation, |delay_ms| {
        thread::sleep(Duration::from_millis(delay_ms));
    })
}

fn retry_sync_with_sleep<T, E, F, S>(config: &RetryConfig, operation: F, sleep: S) -> Result<T, E>
where
    E: Retryable + Debug,
    F: Fn() -> Result<T, E>,
    S: Fn(u64),
{
    let mut retries = 0;

    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if !error.is_transient() => return Err(error),
            Err(error) if retries >= config.max_retries => return Err(error),
            Err(error) => {
                let delay_ms = next_delay_ms(config, retries);
                let next_attempt = retries + 2;
                warn!(
                    retry = retries + 1,
                    next_attempt,
                    delay_ms,
                    error = ?error,
                    "transient failure, retrying operation"
                );
                sleep(delay_ms);
                retries += 1;
            }
        }
    }
}

fn next_delay_ms(config: &RetryConfig, retry_index: u32) -> u64 {
    let multiplier = 1_u64.checked_shl(retry_index).unwrap_or(u64::MAX);
    let base_delay = config.base_delay_ms.saturating_mul(multiplier);
    let capped_delay = base_delay.min(config.max_delay_ms);
    if config.jitter {
        jitter_delay_ms(capped_delay)
    } else {
        capped_delay
    }
}

fn jitter_delay_ms(delay_ms: u64) -> u64 {
    let jitter_span = delay_ms / 4;
    if jitter_span == 0 {
        return delay_ms;
    }

    let max_offset = jitter_span.saturating_mul(2);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    let offset = nanos % (max_offset + 1);

    delay_ms.saturating_sub(jitter_span).saturating_add(offset)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::{RetryConfig, Retryable, next_delay_ms, retry_sync, retry_sync_with_sleep};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum TestError {
        Transient(u32),
        Permanent(u32),
    }

    impl Retryable for TestError {
        fn is_transient(&self) -> bool {
            matches!(self, Self::Transient(_))
        }
    }

    #[test]
    fn retry_returns_success_on_first_attempt() {
        let config = RetryConfig::default();
        let calls = AtomicU32::new(0);

        let result = retry_sync(&config, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, TestError>("ok")
        });

        assert_eq!(result, Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_retries_transient_errors_until_exhausted() {
        let config = RetryConfig {
            max_retries: 3,
            base_delay_ms: 0,
            max_delay_ms: 0,
            jitter: false,
        };
        let calls = AtomicU32::new(0);

        let result = retry_sync(&config, || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            Err::<(), _>(TestError::Transient(attempt))
        });

        assert_eq!(result, Err(TestError::Transient(4)));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn retry_returns_permanent_error_without_retrying() {
        let config = RetryConfig::default();
        let calls = AtomicU32::new(0);

        let result = retry_sync(&config, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(TestError::Permanent(1))
        });

        assert_eq!(result, Err(TestError::Permanent(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn retry_succeeds_after_transient_failures() {
        let config = RetryConfig {
            max_retries: 4,
            base_delay_ms: 0,
            max_delay_ms: 0,
            jitter: false,
        };
        let calls = AtomicU32::new(0);

        let result = retry_sync(&config, || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
            if attempt < 3 {
                Err(TestError::Transient(attempt))
            } else {
                Ok("recovered")
            }
        });

        assert_eq!(result, Ok("recovered"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn backoff_delay_grows_exponentially_and_caps() {
        let config = RetryConfig {
            max_retries: 5,
            base_delay_ms: 100,
            max_delay_ms: 250,
            jitter: false,
        };

        assert_eq!(next_delay_ms(&config, 0), 100);
        assert_eq!(next_delay_ms(&config, 1), 200);
        assert_eq!(next_delay_ms(&config, 2), 250);
        assert_eq!(next_delay_ms(&config, 3), 250);
    }

    #[test]
    fn no_retry_config_never_retries() {
        let config = RetryConfig::no_retry();
        let calls = AtomicU32::new(0);
        let sleep_calls = AtomicU32::new(0);

        let result = retry_sync_with_sleep(
            &config,
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(TestError::Transient(1))
            },
            |_| {
                sleep_calls.fetch_add(1, Ordering::SeqCst);
            },
        );

        assert_eq!(result, Err(TestError::Transient(1)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(sleep_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn default_config_matches_expected_values() {
        let config = RetryConfig::default();

        assert_eq!(config.max_retries, 3);
        assert_eq!(config.base_delay_ms, 100);
        assert_eq!(config.max_delay_ms, 5_000);
        assert!(config.jitter);
    }

    #[test]
    fn preset_configs_match_expected_values() {
        let fast = RetryConfig::fast();
        assert_eq!(fast.max_retries, 2);
        assert_eq!(fast.base_delay_ms, 50);
        assert_eq!(fast.max_delay_ms, 500);
        assert!(fast.jitter);

        let conservative = RetryConfig::conservative();
        assert_eq!(conservative.max_retries, 5);
        assert_eq!(conservative.base_delay_ms, 200);
        assert_eq!(conservative.max_delay_ms, 10_000);
        assert!(conservative.jitter);
    }
}
