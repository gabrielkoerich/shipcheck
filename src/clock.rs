use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// With `SHIPCHECK_TIMINGS` set, prints how long a phase of a job took, for measuring overhead
pub(crate) fn timing(file: &str, job: &str, phase: &str, since: Instant) {
    if std::env::var_os("SHIPCHECK_TIMINGS").is_some() {
        eprintln!(
            "timing {file}/{job} {phase} {:.3}",
            since.elapsed().as_secs_f64()
        );
    }
}

/// `6m07s`, or `42s` under a minute
pub fn clock(secs: u64) -> String {
    match secs / 60 {
        0 => format!("{secs}s"),
        m => format!("{m}m{:02}s", secs % 60),
    }
}
