use std::{cmp::Ordering, iter::Peekable, str::Chars, time::SystemTime};

/// Natural ordering so that `page2.jpg` sorts before `page10.jpg`.
/// Implemented with zero heap allocations.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let ord = compare_numeric_run(&mut ai, &mut bi);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    let mut x_it = x.to_lowercase();
                    let mut y_it = y.to_lowercase();
                    loop {
                        match (x_it.next(), y_it.next()) {
                            (None, None) => break,
                            (None, Some(_)) => return Ordering::Less,
                            (Some(_), None) => return Ordering::Greater,
                            (Some(cx), Some(cy)) => {
                                if cx != cy {
                                    return cx.cmp(&cy);
                                }
                            }
                        }
                    }
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Compare a run of digits numerically without any heap allocations.
fn compare_numeric_run(ai: &mut Peekable<Chars<'_>>, bi: &mut Peekable<Chars<'_>>) -> Ordering {
    // 1. Skip leading zeros
    while ai.peek().copied() == Some('0') {
        ai.next();
    }
    while bi.peek().copied() == Some('0') {
        bi.next();
    }

    // 2. Count significant digits and compare
    let mut first_diff = Ordering::Equal;

    loop {
        let a_digit = ai.peek().copied().filter(|c| c.is_ascii_digit());
        let b_digit = bi.peek().copied().filter(|c| c.is_ascii_digit());

        match (a_digit, b_digit) {
            (Some(da), Some(db)) => {
                if first_diff == Ordering::Equal && da != db {
                    first_diff = da.cmp(&db);
                }
                ai.next();
                bi.next();
            }
            (Some(_), None) => {
                while ai.peek().copied().is_some_and(|c| c.is_ascii_digit()) {
                    ai.next();
                }
                return Ordering::Greater;
            }
            (None, Some(_)) => {
                while bi.peek().copied().is_some_and(|c| c.is_ascii_digit()) {
                    bi.next();
                }
                return Ordering::Less;
            }
            (None, None) => {
                return first_diff;
            }
        }
    }
}

/// FNV-1a. Unlike `DefaultHasher` its output is stable across Rust versions,
/// which matters because it is used for ids that get persisted.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp.
pub fn format_date(secs: u64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Human readable file size, e.g. `42.3 MB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(951_782_400), "2000-02-29");
        assert_eq!(format_date(1_700_000_000), "2023-11-14");
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn sorts_numbers_naturally() {
        let mut names = vec!["p10.jpg", "p2.jpg", "p1.jpg", "p02.jpg", "P3.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["p1.jpg", "p2.jpg", "p02.jpg", "P3.jpg", "p10.jpg"]
        );
    }

    #[test]
    fn folders_group_before_pages() {
        let mut names = vec!["ch2/001.png", "ch10/001.png", "ch1/010.png", "ch1/002.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec!["ch1/002.png", "ch1/010.png", "ch2/001.png", "ch10/001.png"]
        );
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
