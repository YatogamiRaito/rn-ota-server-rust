pub const NUMERIC_COHORT_SIZE: i32 = 1000;
pub const DEFAULT_ROLLOUT_COHORT_COUNT: i32 = 1000;
pub const MAX_COHORT_LENGTH: usize = 64;
pub const INVALID_COHORT_ERROR_MESSAGE: &str =
    "Invalid cohort. Use 1-1000 or a lowercase slug without spaces, up to 64 characters.";

pub fn positive_mod(value: i32, modulus: i32) -> i32 {
    ((value % modulus) + modulus) % modulus
}

pub fn hash_string(value: &str) -> i32 {
    let mut hash: i32 = 0;
    for c in value.chars() {
        let char_code = c as u32 as i32;
        // JS equivalent: hash = (hash << 5) - hash + char_code;
        // wrapping_shl(5) shifts by 5 wrappingly.
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(char_code);
    }
    hash
}

pub fn gcd(a: i32, b: i32) -> i32 {
    let mut x = a.abs();
    let mut y = b.abs();
    while y != 0 {
        let next = x % y;
        x = y;
        y = next;
    }
    x
}

pub fn modular_inverse(value: i32, modulus: i32) -> Result<i32, String> {
    let mut t = 0;
    let mut new_t = 1;
    let mut r = modulus;
    let mut new_r = positive_mod(value, modulus);
    while new_r != 0 {
        let quotient = r / new_r;
        let temp_t = t;
        t = new_t;
        new_t = temp_t - quotient * new_t;

        let temp_r = r;
        r = new_r;
        new_r = temp_r - quotient * new_r;
    }
    if r > 1 {
        return Err(format!("No modular inverse for {} mod {}", value, modulus));
    }
    Ok(positive_mod(t, modulus))
}

pub struct RolloutShuffleParameters {
    pub multiplier: i32,
    pub offset: i32,
    pub inverse_multiplier: i32,
}

pub fn get_rollout_shuffle_parameters(bundle_id: &str) -> RolloutShuffleParameters {
    let mut multiplier = positive_mod(hash_string(&format!("{}:multiplier", bundle_id)), 997);
    if multiplier == 0 {
        multiplier = 1;
    }
    while gcd(multiplier, NUMERIC_COHORT_SIZE) != 1 {
        multiplier = positive_mod(multiplier + 1, NUMERIC_COHORT_SIZE);
        if multiplier == 0 {
            multiplier = 1;
        }
    }
    let offset = positive_mod(
        hash_string(&format!("{}:offset", bundle_id)),
        NUMERIC_COHORT_SIZE,
    );

    // Safety check: mod inverse always exists because multiplier has gcd(multiplier, 1000) == 1
    let inverse_multiplier = modular_inverse(multiplier, NUMERIC_COHORT_SIZE).unwrap_or(1);

    RolloutShuffleParameters {
        multiplier,
        offset,
        inverse_multiplier,
    }
}

pub fn normalize_rollout_cohort_count(rollout_cohort_count: Option<i32>) -> i32 {
    match rollout_cohort_count {
        None => DEFAULT_ROLLOUT_COHORT_COUNT,
        Some(count) => count.clamp(0, 1000),
    }
}

pub fn parse_numeric_cohort_value(cohort: &str) -> Option<i32> {
    if !cohort.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let parsed: i32 = cohort.parse().ok()?;
    if !(1..=1000).contains(&parsed) {
        return None;
    }
    Some(parsed)
}

pub fn normalize_cohort_value(cohort: &str) -> String {
    let normalized = cohort.trim().to_lowercase();
    if let Some(numeric_cohort) = parse_numeric_cohort_value(&normalized) {
        return numeric_cohort.to_string();
    }
    normalized
}

pub fn get_numeric_cohort_value(cohort: &str) -> Option<i32> {
    parse_numeric_cohort_value(&normalize_cohort_value(cohort))
}

pub fn is_custom_cohort(cohort: &str) -> bool {
    let normalized = normalize_cohort_value(cohort);
    !normalized.is_empty()
        && normalized.len() <= MAX_COHORT_LENGTH
        && !normalized.chars().all(|c| c.is_ascii_digit())
        && normalized
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn is_valid_cohort(cohort: &str) -> bool {
    let normalized = normalize_cohort_value(cohort);
    parse_numeric_cohort_value(&normalized).is_some() || is_custom_cohort(&normalized)
}

pub fn get_numeric_cohort_rollout_position(bundle_id: &str, cohort_value: i32) -> i32 {
    if !(1..=1000).contains(&cohort_value) {
        return 0;
    }
    let params = get_rollout_shuffle_parameters(bundle_id);
    positive_mod(
        params.inverse_multiplier * (cohort_value - 1 - params.offset),
        NUMERIC_COHORT_SIZE,
    )
}

pub fn is_cohort_eligible_for_update(
    bundle_id: &str,
    cohort: Option<&str>,
    rollout_cohort_count: Option<i32>,
    target_cohorts: Option<&[String]>,
) -> bool {
    let normalized_cohort = cohort.map(normalize_cohort_value);

    // Target Cohorts match
    if let (Some(ref norm_c), Some(targets)) = (&normalized_cohort, target_cohorts) {
        let normalized_targets: Vec<String> =
            targets.iter().map(|t| normalize_cohort_value(t)).collect();
        if normalized_targets.contains(norm_c) {
            return true;
        }
    }

    let normalized_rollout_count = normalize_rollout_cohort_count(rollout_cohort_count);
    if normalized_rollout_count <= 0 {
        return false;
    }

    // If client cohort is not defined, we only target fully rolled out (100%) bundles
    let Some(ref norm_c) = normalized_cohort else {
        return normalized_rollout_count >= NUMERIC_COHORT_SIZE;
    };

    // Numeric cohort check
    let Some(numeric_cohort) = get_numeric_cohort_value(norm_c) else {
        return false;
    };

    if normalized_rollout_count >= 1000 {
        return true;
    }

    get_numeric_cohort_rollout_position(bundle_id, numeric_cohort) < normalized_rollout_count
}
