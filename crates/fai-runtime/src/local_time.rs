//! Reentrant conversion of system time into the local UTC offset.

use std::mem::MaybeUninit;

pub(crate) fn current_offset() -> i64 {
    // SAFETY: a null output pointer asks time() only for its return value.
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    if now == -1 { 0 } else { offset_at(now).unwrap_or(0) }
}

/// Converts into caller-owned storage; no pointer to libc's shared calendar
/// buffer escapes or is passed to a mutating conversion routine.
fn calendar(timestamp: libc::time_t, local: bool) -> Option<libc::tm> {
    let mut result = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: timestamp is readable and result has the platform's actual tm
    // layout. Each conversion writes only this call's output storage. Failure is
    // checked before assuming initialization; zeroed extension fields are valid.
    unsafe {
        #[cfg(unix)]
        let success = if local {
            !libc::localtime_r(&timestamp, result.as_mut_ptr()).is_null()
        } else {
            !libc::gmtime_r(&timestamp, result.as_mut_ptr()).is_null()
        };
        #[cfg(windows)]
        let success = if local {
            libc::localtime_s(result.as_mut_ptr(), &timestamp) == 0
        } else {
            libc::gmtime_s(result.as_mut_ptr(), &timestamp) == 0
        };
        success.then(|| result.assume_init())
    }
}

fn offset_at(timestamp: libc::time_t) -> Option<i64> {
    let local = calendar(timestamp, true)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        // c_long is 32 or 64 bits depending on the target ABI.
        #[allow(clippy::unnecessary_cast)]
        Some(local.tm_gmtoff as i64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let utc = calendar(timestamp, false)?;
        calendar_seconds(&local).checked_sub(calendar_seconds(&utc))
    }
}

/// Gregorian day counts avoid timegm/_mkgmtime and handle year-boundary offsets.
/// A successful libc conversion has bounded c_int fields, so these intermediates
/// fit i64 even at the extremes of tm_year.
#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
fn calendar_seconds(value: &libc::tm) -> i64 {
    let preceding_year = i64::from(value.tm_year) + 1899;
    let days = 365 * preceding_year + preceding_year.div_euclid(4) - preceding_year.div_euclid(100)
        + preceding_year.div_euclid(400)
        + i64::from(value.tm_yday);
    days * 86_400
        + i64::from(value.tm_hour) * 3600
        + i64::from(value.tm_min) * 60
        + i64::from(value.tm_sec)
}

#[cfg(test)]
mod tests {
    use wait_timeout::ChildExt;

    use super::*;

    #[track_caller]
    fn in_zone(zone: &str, timestamp: i64, expected: i64, stress: bool) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "local_time::tests::zone_worker", "--nocapture"])
            .env("TZ", zone)
            .env("FAI_TZ_TIMESTAMP", timestamp.to_string())
            .env("FAI_TZ_EXPECTED", expected.to_string())
            .env("FAI_TZ_STRESS", if stress { "1" } else { "0" })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let finished = child.wait_timeout(std::time::Duration::from_secs(30)).unwrap().is_some();
        if !finished {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            finished && output.status.success(),
            "{zone}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn eastern() -> &'static str {
        if cfg!(windows) { "EST5EDT" } else { "EST5EDT,M3.2.0/2,M11.1.0/2" }
    }

    #[test]
    fn utc_has_zero_offset() {
        in_zone("UTC0", 1705320000, 0, false);
    }
    #[test]
    fn positive_half_hour_offset() {
        in_zone("IST-5:30", 1705320000, 19800, false);
    }
    #[test]
    fn westward_offset_crosses_the_previous_year() {
        in_zone("PST8", 1704067200, -28800, false);
    }
    #[test]
    fn eastward_offset_crosses_the_next_year() {
        in_zone("XST-14", 1704045600, 50400, false);
    }
    #[test]
    fn winter_standard_time() {
        in_zone(eastern(), 1705320000, -18000, false);
    }
    #[test]
    fn summer_daylight_time() {
        in_zone(eastern(), 1721044800, -14400, false);
    }
    #[test]
    fn before_spring_transition() {
        in_zone(eastern(), 1710053999, -18000, false);
    }
    #[test]
    fn after_spring_transition() {
        in_zone(eastern(), 1710054000, -14400, false);
    }
    #[test]
    fn before_autumn_transition() {
        in_zone(eastern(), 1730613599, -14400, false);
    }
    #[test]
    fn after_autumn_transition() {
        in_zone(eastern(), 1730613600, -18000, false);
    }
    #[test]
    fn concurrent_conversions_have_independent_storage() {
        in_zone("PST8", 1704067200, -28800, true);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn unrepresentable_time_is_reported_as_missing() {
        assert!(offset_at(libc::time_t::MAX).is_none());
    }

    #[test]
    fn zone_worker() {
        let Ok(timestamp) = std::env::var("FAI_TZ_TIMESTAMP") else { return };
        let timestamp = timestamp.parse::<libc::time_t>().unwrap();
        let expected = std::env::var("FAI_TZ_EXPECTED").unwrap().parse::<i64>().unwrap();
        #[cfg(windows)]
        // SAFETY: this isolated worker sets TZ before process startup and initializes
        // the CRT timezone before starting any conversion threads.
        unsafe {
            libc::tzset();
        }
        assert_eq!(offset_at(timestamp), Some(expected));
        if std::env::var("TZ").as_deref() == Ok("UTC0") {
            let value = crate::fai_clock_local_offset(crate::FAI_UNIT);
            assert_eq!(crate::read_int(value), 0);
            crate::fai_drop(value);
        }
        let local = calendar(timestamp, true).unwrap();
        let utc = calendar(timestamp, false).unwrap();
        assert_eq!(calendar_seconds(&local) - calendar_seconds(&utc), expected);
        if std::env::var("FAI_TZ_STRESS").as_deref() == Ok("1") {
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                let barrier = &barrier;
                #[cfg(unix)]
                let foreign = scope.spawn(move || {
                    for iteration in 0..4096 {
                        let at = timestamp + iteration * 3600;
                        // SAFETY: only this thread uses the shared-buffer API,
                        // and it never reads the returned pointer. Runtime calls
                        // use independent storage even alongside native users.
                        let _ = unsafe { libc::localtime(&at) };
                    }
                });
                let threads: Vec<_> = (0..8)
                    .map(|thread| {
                        scope.spawn(move || {
                            barrier.wait();
                            for iteration in 0..2048 {
                                let at = timestamp + thread * 86400 + iteration * 60;
                                assert_eq!(offset_at(at), Some(expected));
                            }
                        })
                    })
                    .collect();
                for thread in threads {
                    thread.join().unwrap();
                }
                #[cfg(unix)]
                foreign.join().unwrap();
            });
        }
    }
}
