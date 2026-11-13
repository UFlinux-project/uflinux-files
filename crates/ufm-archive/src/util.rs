//! Small conversions shared by the backends.
//!
//! Every format stores a date differently — zip as a packed local-time struct,
//! tar as octal seconds, 7z as `YYYY-MM-DD HH:MM:SS` printed in whatever the
//! child's `TZ` says — and all of them end up as a [`SystemTime`] here.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::toc::NodeKind;

/// Convert a civil date to days since the Unix epoch, Howard Hinnant's
/// `days_from_civil`, so no date library is linked for three arithmetic
/// operations. The shifted-era trick is what makes leap years fall out without a
/// table: moving March to the start of the year puts February, the only irregular
/// month, at the end.
pub(crate) fn days_from_civil(year: u32, month: i64, day: i64) -> Option<u64> {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    u64::try_from(era * 146_097 + day_of_era - 719_468).ok()
}

/// Seconds and nanoseconds since the epoch. A time before 1970 has no
/// [`SystemTime`], and formats disagree about whether it is even legal, so it is
/// reported as unknown rather than clamped to the epoch and shown as 1970.
pub(crate) fn epoch(secs: u64, nanos: u32) -> Option<SystemTime> {
    UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
}

/// Seconds since the epoch from a signed value, `None` for anything negative.
pub(crate) fn epoch_secs(secs: i64) -> Option<SystemTime> {
    epoch(u64::try_from(secs).ok()?, 0)
}

/// The `S_IF*` bits for a node kind, so a mode can be shown as `drwxr-xr-x`
/// rather than as a bare permission number. Formats that store only permissions
/// — tar's mode field, pax's `mode`, 7z's `Attributes` — need these to complete it.
#[must_use]
pub(crate) const fn mode_bits(kind: NodeKind) -> u32 {
    match kind {
        NodeKind::Dir => 0o040_000,
        NodeKind::Symlink => 0o120_000,
        NodeKind::Special => 0o020_000,
        NodeKind::File => 0o100_000,
    }
}

/// Complete a permission-only mode with the type bits for its kind.
#[must_use]
pub(crate) const fn full_mode(kind: NodeKind, permissions: u32) -> u32 {
    mode_bits(kind) | (permissions & 0o7777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_convert_to_days() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1970, 1, 2), Some(1));
        assert_eq!(days_from_civil(1969, 12, 31), None, "before the epoch");
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(days_from_civil(2024, 2, 29), Some(19_782));
        assert_eq!(days_from_civil(2026, 11, 13), Some(20_770));
    }

    #[test]
    fn a_full_timestamp_lands_on_the_right_instant() {
        // 2026-11-13T16:25:48Z, checked against `date -d`.
        let days = days_from_civil(2026, 11, 13).expect("in range");
        let at = epoch(days * 86_400 + 16 * 3_600 + 25 * 60 + 48, 0).expect("in range");
        assert_eq!(
            at.duration_since(UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_794_587_148
        );
    }

    #[test]
    fn a_time_before_the_epoch_is_unknown_rather_than_zero() {
        assert_eq!(epoch_secs(-1), None);
        assert_eq!(epoch_secs(0), Some(UNIX_EPOCH));
    }

    #[test]
    fn modes_carry_their_type_bits() {
        assert_eq!(full_mode(NodeKind::Dir, 0o755), 0o040_755);
        assert_eq!(full_mode(NodeKind::File, 0o644), 0o100_644);
        assert_eq!(full_mode(NodeKind::Symlink, 0o777), 0o120_777);
        // A setuid bit survives, a stray type field does not.
        assert_eq!(full_mode(NodeKind::File, 0o1755), 0o101_755);
        assert_eq!(full_mode(NodeKind::File, 0o17_644), 0o107_644);
    }
}
