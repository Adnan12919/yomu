use std::{cmp::Ordering, iter::Peekable, str::Chars, time::SystemTime};

/// Natural ordering so that `page2.jpg` sorts before `page10.jpg`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let na = take_number(&mut ai);
                let nb = take_number(&mut bi);
                let ord = na.len().cmp(&nb.len()).then_with(|| na.cmp(&nb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// Consume a run of ASCII digits and return it without leading zeros.
fn take_number(it: &mut Peekable<Chars<'_>>) -> String {
    let mut digits = String::new();
    while let Some(c) = it.peek().copied() {
        if !c.is_ascii_digit() {
            break;
        }
        digits.push(c);
        it.next();
    }
    let trimmed = digits.trim_start_matches('0');
    trimmed.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_numbers_naturally() {
        let mut names = vec!["p10.jpg", "p2.jpg", "p1.jpg", "p02.jpg", "P3.jpg"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["p1.jpg", "p2.jpg", "p02.jpg", "P3.jpg", "p10.jpg"]);
    }

    #[test]
    fn folders_group_before_pages() {
        let mut names = vec!["ch2/001.png", "ch10/001.png", "ch1/010.png", "ch1/002.png"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(names, vec!["ch1/002.png", "ch1/010.png", "ch2/001.png", "ch10/001.png"]);
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
