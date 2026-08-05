use rn_ota_server_rust::cohort;

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct HashFixture {
    input: String,
    output: i32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GcdFixture {
    a: i32,
    b: i32,
    output: i32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModInverseFixture {
    value: i32,
    modulus: i32,
    output: i32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RolloutPositionFixture {
    bundle_id: String,
    cohort_value: i32,
    output: i32,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EligibilityFixture {
    bundle_id: String,
    cohort: Option<String>,
    rollout_cohort_count: Option<i32>,
    target_cohorts: Option<Vec<String>>,
    result: bool,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Fixtures {
    hash_string: Vec<HashFixture>,
    gcd: Vec<GcdFixture>,
    modular_inverse: Vec<ModInverseFixture>,
    rollout_position: Vec<RolloutPositionFixture>,
    eligibility: Vec<EligibilityFixture>,
}

#[test]
fn test_cohort_parity_with_fixtures() {
    let fixtures_json =
        std::fs::read_to_string("tests/fixtures.json").expect("Failed to read tests/fixtures.json");

    let fixtures: Fixtures =
        serde_json::from_str(&fixtures_json).expect("Failed to parse tests/fixtures.json");

    // 1. Hash string test
    for f in fixtures.hash_string {
        let rust_output = cohort::hash_string(&f.input);
        assert_eq!(
            rust_output, f.output,
            "hash_string parity failed for input: '{}'",
            f.input
        );
    }

    // 2. GCD test
    for f in fixtures.gcd {
        let rust_output = cohort::gcd(f.a, f.b);
        assert_eq!(
            rust_output, f.output,
            "gcd parity failed for ({}, {})",
            f.a, f.b
        );
    }

    // 3. Modular Inverse test
    for f in fixtures.modular_inverse {
        let rust_output = cohort::modular_inverse(f.value, f.modulus).unwrap();
        assert_eq!(
            rust_output, f.output,
            "modular_inverse parity failed for {} mod {}",
            f.value, f.modulus
        );
    }

    // 4. Rollout Position test
    for f in fixtures.rollout_position {
        let rust_output = cohort::get_numeric_cohort_rollout_position(&f.bundle_id, f.cohort_value);
        assert_eq!(
            rust_output, f.output,
            "get_numeric_cohort_rollout_position parity failed for bundle_id: '{}', cohort_value: {}",
            f.bundle_id, f.cohort_value
        );
    }

    // 5. Eligibility test
    for f in fixtures.eligibility {
        let cohort_ref = f.cohort.as_deref();
        let target_cohorts_ref = f.target_cohorts.as_deref();

        let rust_output = cohort::is_cohort_eligible_for_update(
            &f.bundle_id,
            cohort_ref,
            f.rollout_cohort_count,
            target_cohorts_ref,
        );

        assert_eq!(
            rust_output, f.result,
            "is_cohort_eligible_for_update parity failed for bundle_id: '{}', cohort: {:?}, rollout_cohort_count: {:?}, target_cohorts: {:?}",
            f.bundle_id, f.cohort, f.rollout_cohort_count, f.target_cohorts
        );
    }
}
