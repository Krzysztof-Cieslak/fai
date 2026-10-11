//! External RSS observations at a native worker's quiescent protocol boundaries.

use std::io;

/// Resident memory and its process high-water mark, in Linux's 1024-byte units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rss {
    /// Resident memory at the observation boundary.
    pub current_kib: u64,
    /// Maximum resident memory since process startup.
    pub peak_kib: u64,
}

impl Rss {
    /// Decodes one `/proc/PID/status` snapshot, rejecting missing or wrong units.
    pub fn parse(status: &str) -> Option<Self> {
        let field = |name| {
            let mut words =
                status.lines().find_map(|line| line.strip_prefix(name))?.split_whitespace();
            let value = words.next()?.parse::<u64>().ok()?;
            (value > 0 && words.next() == Some("kB") && words.next().is_none()).then_some(value)
        };
        let value = Self { current_kib: field("VmRSS:")?, peak_kib: field("VmHWM:")? };
        (value.peak_kib >= value.current_kib).then_some(value)
    }

    /// Reads a live child without allocating observation buffers in that child.
    pub fn observe(pid: u32) -> io::Result<Self> {
        Self::parse(&std::fs::read_to_string(format!("/proc/{pid}/status"))?).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing or invalid child RSS")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_and_peak_are_distinct_observations() {
        assert_eq!(
            Rss::parse("Name:\tworker\nVmHWM:\t4096 kB\nVmRSS:\t2048 kB\n"),
            Some(Rss { current_kib: 2048, peak_kib: 4096 })
        );
    }

    #[test]
    fn missing_current_rss_is_not_zero() {
        assert_eq!(Rss::parse("VmHWM: 4096 kB\n"), None);
    }

    #[test]
    fn wrong_units_are_rejected() {
        assert_eq!(Rss::parse("VmHWM: 4096 bytes\nVmRSS: 2048 kB\n"), None);
    }

    #[test]
    fn impossible_peak_is_rejected() {
        assert_eq!(Rss::parse("VmHWM: 1024 kB\nVmRSS: 2048 kB\n"), None);
    }
}
