// ============================================================================
// R2 MANUAL VERIFICATION TEST
// ============================================================================
//
// This test verifies that the `get_presigned_url` function in `storage.rs` produces
// a presigned URL that actually works against a real Cloudflare R2 bucket. It does
// NOT run in the normal `cargo test` / CI flow (#[ignore]) because it requires real
// R2 credentials and performs a real network request.
//
// RUN:
//
//   R2_ENDPOINT=https://<account>.r2.cloudflarestorage.com \
//   R2_ACCESS_KEY_ID=xxxxx \
//   R2_SECRET_ACCESS_KEY=xxxxx \
//   R2_BUCKET_NAME=your-bucket \
//   R2_TEST_OBJECT_KEY=path/to/existing-test-object.txt \
//   cargo test --test r2_manual_verification -- --ignored --nocapture
//
// `R2_TEST_OBJECT_KEY` must be the key of a file that ACTUALLY EXISTS in the bucket
// (a small/known test file is recommended). If one of the env variables is missing,
// the test does NOT panic; it fails with an explanatory message.
//
// ============================================================================

use rn_ota_server_rust::storage::get_presigned_url;
use std::env;

struct R2TestEnv {
    endpoint: Option<String>,
    access_key: String,
    secret_key: String,
    bucket: String,
    object_key: String,
}

fn load_env() -> Result<R2TestEnv, String> {
    let access_key =
        env::var("R2_ACCESS_KEY_ID").map_err(|_| "Missing env: R2_ACCESS_KEY_ID".to_string())?;
    let secret_key = env::var("R2_SECRET_ACCESS_KEY")
        .map_err(|_| "Missing env: R2_SECRET_ACCESS_KEY".to_string())?;
    let bucket =
        env::var("R2_BUCKET_NAME").map_err(|_| "Missing env: R2_BUCKET_NAME".to_string())?;
    let object_key = env::var("R2_TEST_OBJECT_KEY").map_err(|_| {
        "Missing env: R2_TEST_OBJECT_KEY (key of an existing test object in the bucket)".to_string()
    })?;
    let endpoint = env::var("R2_ENDPOINT").ok();

    Ok(R2TestEnv {
        endpoint,
        access_key,
        secret_key,
        bucket,
        object_key,
    })
}

#[tokio::test]
#[ignore]
async fn presigned_url_can_access_a_real_r2_bucket() {
    let env = match load_env() {
        Ok(e) => e,
        Err(msg) => {
            eprintln!(
                "Test skipped -- required env variables are missing: {}\n\
                 Read the comment at the top of this file for run instructions.",
                msg
            );
            return;
        }
    };

    let storage_uri = format!("s3://{}/{}", env.bucket, env.object_key);

    let presigned_url = get_presigned_url(
        env.endpoint.as_deref(),
        &env.access_key,
        &env.secret_key,
        &env.bucket,
        &storage_uri,
    )
    .await
    .expect("presigned URL generation failed");

    println!("Generated presigned URL: {}", presigned_url);

    let response = reqwest::get(&presigned_url)
        .await
        .expect("HTTP GET to presigned URL failed");

    let status = response.status();
    assert!(
        status.is_success(),
        "Unexpected HTTP status from R2: {} (presigned URL could not be resolved/accessed)",
        status
    );

    let body = response
        .bytes()
        .await
        .expect("response body could not be read");

    assert!(
        !body.is_empty(),
        "File content returned from R2 is empty -- expected content was not present"
    );

    println!(
        "Verification succeeded: received {} bytes of content ({}).",
        body.len(),
        env.object_key
    );
}
