// Behavior parity with Node's real `semver` package (coerce + satisfies) is verified by
// tests/semver_parity_tests.rs + tests/fixtures/semver_fixtures.json.
// Fixture generator: tests/generate_semver_fixtures.mjs.
//
// Known limitation: node-semver rejects/truncates numeric components against JS's `Number`
// safe-integer limit (2^53-1, ~16 digits); this implementation uses Rust `u64` (~20 digits),
// so bit-for-bit identical behavior with node is NOT GUARANTEED for components with 16+
// digits (this never occurs in real application version strings).
// The related fixtures are marked #[ignore] in tests/semver_parity_tests.rs.

fn parse_no_leading_zero(s: &str) -> Option<u64> {
    // semver numeric identifier rule: a leading zero is invalid except for "0" itself
    // (e.g. "01", "00" are rejected; in node-semver this invalidates the ENTIRE coerce/range).
    if s.len() > 1 && s.starts_with('0') {
        return None;
    }
    s.parse::<u64>().ok()
}

pub fn coerce_version(s: &str) -> Option<semver::Version> {
    // Find the first sequence of digits
    let first_digit_idx = s.find(|c: char| c.is_ascii_digit())?;
    let s = &s[first_digit_idx..];

    // Extract dot-separated digit segments
    let mut segments: Vec<u64> = Vec::new();
    let mut current_segment = String::new();

    for c in s.chars() {
        if c.is_ascii_digit() {
            current_segment.push(c);
        } else if c == '.' {
            if current_segment.is_empty() {
                break;
            }
            segments.push(parse_no_leading_zero(&current_segment)?);
            current_segment = String::new();
            if segments.len() == 3 {
                break;
            }
        } else {
            break;
        }
    }

    if !current_segment.is_empty() && segments.len() < 3 {
        segments.push(parse_no_leading_zero(&current_segment)?);
    }

    if segments.is_empty() {
        return None;
    }

    let major = segments[0];
    let minor = segments.get(1).copied().unwrap_or(0);
    let patch = segments.get(2).copied().unwrap_or(0);

    Some(semver::Version::new(major, minor, patch))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Triple(u64, u64, u64);

/// The parsed version part of a range comparator. `None` fields are missing/wildcard
/// (x, X, *) segments -- they form the basis of npm's "X-Range" rules.
#[derive(Debug, Clone, Copy)]
struct PartialVersion {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    has_prerelease: bool,
}

fn is_full(p: &PartialVersion) -> bool {
    p.major.is_some() && p.minor.is_some() && p.patch.is_some()
}

fn fill_zero(p: &PartialVersion) -> Triple {
    Triple(
        p.major.unwrap_or(0),
        p.minor.unwrap_or(0),
        p.patch.unwrap_or(0),
    )
}

/// npm's partial+operator expansion rule: increments the LAST specified segment by one
/// and zeroes out everything after it (e.g. ">1.4" -> ">=1.5.0", "<=1.2" -> "<1.3.0").
fn increment_last_specified(p: &PartialVersion) -> Triple {
    match (p.minor, p.patch) {
        (None, _) => Triple(p.major.unwrap_or(0) + 1, 0, 0),
        (Some(minor), None) => Triple(p.major.unwrap_or(0), minor + 1, 0),
        (Some(minor), Some(patch)) => Triple(p.major.unwrap_or(0), minor, patch + 1),
    }
}

fn parse_partial_version(raw: &str) -> Option<PartialVersion> {
    let raw = raw.trim();
    let raw = raw.strip_prefix(['v', 'V']).unwrap_or(raw);
    if raw.is_empty() {
        return None;
    }

    let (version_part, has_prerelease) = match raw.find(['-', '+']) {
        Some(idx) => (&raw[..idx], raw.as_bytes()[idx] == b'-'),
        None => (raw, false),
    };
    if version_part.is_empty() {
        return None;
    }

    let mut segments: [Option<u64>; 3] = [None, None, None];
    let mut wildcard_hit = false;
    for (i, part) in version_part.split('.').enumerate() {
        if i >= 3 {
            // Extra segment (e.g. "1.2.3.4") -- node-semver treats this as an invalid range.
            return None;
        }
        if wildcard_hit {
            continue;
        }
        if part.is_empty() {
            return None;
        }
        if part == "x" || part == "X" || part == "*" {
            wildcard_hit = true;
            continue;
        }
        segments[i] = Some(parse_no_leading_zero(part)?);
    }

    segments[0]?;
    Some(PartialVersion {
        major: segments[0],
        minor: segments[1],
        patch: segments[2],
        has_prerelease,
    })
}

/// Caret (^) lower/upper bounds -- npm rules: if major>0 it pins the major; for
/// 0.x.y / 0.0.z it pins progressively tighter (at the minor / patch level).
fn caret_bounds(p: &PartialVersion) -> (Triple, Triple) {
    let major = p.major.unwrap_or(0);
    if major > 0 {
        return (
            Triple(major, p.minor.unwrap_or(0), p.patch.unwrap_or(0)),
            Triple(major + 1, 0, 0),
        );
    }
    match p.minor {
        None => (Triple(0, 0, 0), Triple(1, 0, 0)),
        Some(minor) => match p.patch {
            None => (Triple(0, minor, 0), Triple(0, minor + 1, 0)),
            Some(patch) if minor == 0 => (Triple(0, 0, patch), Triple(0, 0, patch + 1)),
            Some(patch) => (Triple(0, minor, patch), Triple(0, minor + 1, 0)),
        },
    }
}

/// Tilde (~) lower/upper bounds -- npm rule: pins the minor if a patch was specified,
/// otherwise pins the major.
fn tilde_bounds(p: &PartialVersion) -> (Triple, Triple) {
    let major = p.major.unwrap_or(0);
    match p.minor {
        None => (Triple(major, 0, 0), Triple(major + 1, 0, 0)),
        Some(minor) => (
            Triple(major, minor, p.patch.unwrap_or(0)),
            Triple(major, minor + 1, 0),
        ),
    }
}

#[derive(Debug, Clone, Copy)]
enum Bound {
    Gte(Triple),
    Gt(Triple, bool),
    Lte(Triple, bool),
    Lt(Triple),
    /// bool: whether the comparator carries a prerelease tag. Since a coerced subject
    /// version never carries a prerelease, a bound with a prerelease tag on the same
    /// numeric triple always counts as "less than" the subject (release > prerelease).
    Eq(Triple, bool),
}

fn eval_bound(subject: Triple, bound: &Bound) -> bool {
    match *bound {
        Bound::Gte(b) => subject >= b,
        Bound::Lt(b) => subject < b,
        Bound::Gt(b, has_prerelease) => {
            if has_prerelease && subject == b {
                true
            } else {
                subject > b
            }
        }
        Bound::Lte(b, has_prerelease) => {
            if has_prerelease && subject == b {
                false
            } else {
                subject <= b
            }
        }
        Bound::Eq(b, has_prerelease) => !has_prerelease && subject == b,
    }
}

fn parse_comparator_bounds(token: &str) -> Option<Vec<Bound>> {
    let token = token.trim();
    if token.is_empty() {
        return Some(vec![]);
    }

    let (op, rest) = if let Some(r) = token.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = token.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = token.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = token.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = token.strip_prefix('^') {
        ("^", r)
    } else if let Some(r) = token.strip_prefix('~') {
        ("~", r)
    } else if let Some(r) = token.strip_prefix('=') {
        ("=", r)
    } else {
        ("", token)
    };

    let rest = rest.trim();
    if rest.is_empty() || rest == "*" || rest.eq_ignore_ascii_case("x") {
        // Wildcard version part: always matches, regardless of the operator.
        return Some(vec![]);
    }

    let p = parse_partial_version(rest)?;

    Some(match op {
        "" | "=" => {
            if is_full(&p) {
                vec![Bound::Eq(
                    Triple(p.major.unwrap(), p.minor.unwrap(), p.patch.unwrap()),
                    p.has_prerelease,
                )]
            } else {
                vec![
                    Bound::Gte(fill_zero(&p)),
                    Bound::Lt(increment_last_specified(&p)),
                ]
            }
        }
        ">=" => vec![Bound::Gte(fill_zero(&p))],
        "<" => vec![Bound::Lt(fill_zero(&p))],
        ">" => {
            if is_full(&p) {
                vec![Bound::Gt(fill_zero(&p), p.has_prerelease)]
            } else {
                vec![Bound::Gte(increment_last_specified(&p))]
            }
        }
        "<=" => {
            if is_full(&p) {
                vec![Bound::Lte(fill_zero(&p), p.has_prerelease)]
            } else {
                vec![Bound::Lt(increment_last_specified(&p))]
            }
        }
        "^" => {
            let (lower, upper) = caret_bounds(&p);
            vec![Bound::Gte(lower), Bound::Lt(upper)]
        }
        "~" => {
            let (lower, upper) = tilde_bounds(&p);
            vec![Bound::Gte(lower), Bound::Lt(upper)]
        }
        _ => unreachable!("unknown operator: {op}"),
    })
}

fn parse_range_clause(clause: &str) -> Option<Vec<Bound>> {
    let clause = clause.trim();
    if clause.is_empty() || clause == "*" || clause.eq_ignore_ascii_case("x") {
        return Some(vec![]);
    }

    // Hyphen range: "1.2.3 - 2.3.4" (lower/upper may be partial, see the npm README).
    if clause.matches(" - ").count() == 1 {
        let idx = clause.find(" - ").unwrap();
        let lower = parse_partial_version(clause[..idx].trim())?;
        let upper = parse_partial_version(clause[idx + 3..].trim())?;
        let lower_bound = Bound::Gte(fill_zero(&lower));
        let upper_bound = if is_full(&upper) {
            Bound::Lte(fill_zero(&upper), upper.has_prerelease)
        } else {
            Bound::Lt(increment_last_specified(&upper))
        };
        return Some(vec![lower_bound, upper_bound]);
    }

    let mut bounds = Vec::new();
    for token in clause.split_whitespace() {
        bounds.extend(parse_comparator_bounds(token)?);
    }
    Some(bounds)
}

/// Parity with npm `semver.satisfies(version, range)`. `version` always comes from
/// `coerce_version` (it carries no prerelease/build); `req_str` is the raw
/// `TARGET_APP_VERSION` range string (supports `*`, `^1.2.3`, `>=1.0.0 <2.0.0`,
/// hyphen ranges, OR groups separated by `||`, and so on).
///
/// In node-semver, if any OR branch (a `||`-separated group) fails to parse, the ENTIRE
/// range is considered invalid (satisfies returns false) -- the same rule applies here:
/// if any branch fails to parse, the function returns false immediately.
pub fn satisfies(version: &semver::Version, req_str: &str) -> bool {
    let trimmed = req_str.trim();
    if trimmed.is_empty() {
        return true;
    }

    let subject = Triple(version.major, version.minor, version.patch);

    let mut clause_bounds = Vec::new();
    for clause in trimmed.split("||") {
        match parse_range_clause(clause) {
            Some(bounds) => clause_bounds.push(bounds),
            None => return false,
        }
    }

    clause_bounds
        .iter()
        .any(|bounds| bounds.iter().all(|b| eval_bound(subject, b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_coerce_version() {
        assert_eq!(
            coerce_version("1.2.3").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert_eq!(
            coerce_version("v1.2.3").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert_eq!(
            coerce_version("1.2").unwrap(),
            semver::Version::new(1, 2, 0)
        );
        assert_eq!(
            coerce_version("abc-1.2.3.4").unwrap(),
            semver::Version::new(1, 2, 3)
        );
        assert_eq!(coerce_version("1").unwrap(), semver::Version::new(1, 0, 0));
        assert!(coerce_version("abc").is_none());
    }

    #[test]
    fn test_coerce_version_rejects_leading_zero() {
        assert!(coerce_version("01.2.3").is_none());
        assert!(coerce_version("1.02.3").is_none());
        assert!(coerce_version("2021.01.02").is_none());
        assert!(coerce_version("0.1.2").is_some());
    }

    #[test]
    fn test_satisfies() {
        let v = semver::Version::new(1, 2, 3);
        assert!(satisfies(&v, ">=1.0.0"));
        assert!(satisfies(&v, "^1.2.0"));
        assert!(satisfies(&v, "1.2.3"));
        assert!(satisfies(&v, "1.2"));
        assert!(satisfies(&v, "1"));
        assert!(satisfies(&v, "*"));
        assert!(!satisfies(&v, ">=2.0.0"));
        assert!(!satisfies(&v, "1.3"));
    }

    #[test]
    fn test_satisfies_bare_full_version_is_exact_not_caret() {
        // In npm semver a bare full version means "=", NOT "^" as it does in Cargo.
        let v = semver::Version::new(1, 5, 0);
        assert!(!satisfies(&v, "1.2.3"));
    }
}
