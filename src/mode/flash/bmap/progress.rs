use std::path::Path;
use std::time::Duration;

use crate::mode::flash::rawio::KIB;

const MIB: u64 = 1024 * KIB;
const PERCENT: u64 = 100;

/// Logs the position in the image at most once per `interval`.
pub(super) struct Progress<'a> {
    destination: &'a Path,
    image_size: u64,
    interval: Duration,
    elapsed: Box<dyn FnMut() -> Duration>,
    next_log: Duration,
}

impl<'a> Progress<'a> {
    pub(super) fn new(
        destination: &'a Path,
        image_size: u64,
        interval: Duration,
        elapsed: Box<dyn FnMut() -> Duration>,
    ) -> Self {
        Self {
            destination,
            image_size,
            interval,
            elapsed,
            next_log: interval,
        }
    }

    pub(super) fn update(&mut self, position: u64) {
        let elapsed = (self.elapsed)();
        if elapsed < self.next_log {
            return;
        }
        self.next_log = elapsed.saturating_add(self.interval);
        let percent = position
            .saturating_mul(PERCENT)
            .checked_div(self.image_size)
            .unwrap_or(PERCENT);
        log::info!(
            "{}: {} of {} MiB ({percent}%)",
            self.destination.display(),
            position / MIB,
            self.image_size / MIB
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::flash::bmap::test_support::{PROGRESS_LABEL, progress_lines};

    #[test]
    fn progress_is_logged_once_per_interval() {
        const STEP: Duration = Duration::from_secs(1);
        const INTERVAL: Duration = Duration::from_secs(3);
        const UPDATES: u32 = 10;
        let lines = progress_lines(|| {
            let mut elapsed = Duration::ZERO;
            let mut progress = Progress::new(
                Path::new(PROGRESS_LABEL),
                4 * MIB,
                INTERVAL,
                Box::new(move || {
                    elapsed += STEP;
                    elapsed
                }),
            );
            for _ in 0..UPDATES {
                progress.update(MIB);
            }
        });
        let expected = (STEP * UPDATES).as_secs() / INTERVAL.as_secs();
        assert_eq!(lines.len() as u64, expected, "{lines:?}");
        assert!(lines[0].ends_with("1 of 4 MiB (25%)"), "{lines:?}");
    }

    #[test]
    fn progress_of_an_empty_image_is_complete() {
        let lines = progress_lines(|| {
            Progress::new(
                Path::new(PROGRESS_LABEL),
                0,
                Duration::ZERO,
                Box::new(|| Duration::ZERO),
            )
            .update(0);
        });
        assert!(lines[0].ends_with("(100%)"), "{lines:?}");
    }
}
