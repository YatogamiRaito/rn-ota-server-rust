use anyhow::{anyhow, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use std::time::Duration;

pub fn parse_s3_uri(uri: &str) -> Option<(String, String)> {
    let url = url::Url::parse(uri).ok()?;
    if url.scheme() != "s3" {
        return None;
    }
    let bucket = url.host_str()?.to_string();
    let key = url.path().trim_start_matches('/').to_string();
    Some((bucket, key))
}

pub async fn get_presigned_url(
    endpoint: Option<&str>,
    access_key: &str,
    secret_key: &str,
    bucket_name: &str,
    storage_uri: &str,
) -> Result<String> {
    let (uri_bucket, key) = parse_s3_uri(storage_uri)
        .ok_or_else(|| anyhow!("Invalid S3 storage URI: {}", storage_uri))?;

    if uri_bucket != bucket_name {
        return Err(anyhow!(
            "Bucket name mismatch: expected '{}', but found '{}'",
            bucket_name,
            uri_bucket
        ));
    }

    let credentials = aws_credential_types::Credentials::new(
        access_key.to_string(),
        secret_key.to_string(),
        None,
        None,
        "StaticCredentials",
    );

    let mut config_builder = aws_sdk_s3::config::Builder::new()
        .credentials_provider(credentials)
        .region(aws_config::Region::new("auto"));

    if let Some(ep) = endpoint {
        // Cloudflare R2 (S3-compatible API) expects path-style addressing
        // (https://<account>.r2.cloudflarestorage.com/<bucket>/<key>).
        // The AWS SDK defaults to virtual-hosted-style (bucket injected into the
        // subdomain), which can produce unresolvable URLs with a custom endpoint (R2).
        config_builder = config_builder.endpoint_url(ep).force_path_style(true);
    }

    let client = aws_sdk_s3::Client::from_conf(config_builder.build());

    let presigned_req = client
        .get_object()
        .bucket(bucket_name)
        .key(key)
        .presigned(PresigningConfig::expires_in(Duration::from_secs(3600))?)
        .await?;

    Ok(presigned_req.uri().to_string())
}

pub async fn read_s3_file(
    endpoint: Option<&str>,
    access_key: &str,
    secret_key: &str,
    bucket_name: &str,
    storage_uri: &str,
) -> Result<String> {
    let (uri_bucket, key) = parse_s3_uri(storage_uri)
        .ok_or_else(|| anyhow!("Invalid S3 storage URI: {}", storage_uri))?;

    if uri_bucket != bucket_name {
        return Err(anyhow!(
            "Bucket name mismatch: expected '{}', but found '{}'",
            bucket_name,
            uri_bucket
        ));
    }

    let credentials = aws_credential_types::Credentials::new(
        access_key.to_string(),
        secret_key.to_string(),
        None,
        None,
        "StaticCredentials",
    );

    let mut config_builder = aws_sdk_s3::config::Builder::new()
        .credentials_provider(credentials)
        .region(aws_config::Region::new("auto"));

    if let Some(ep) = endpoint {
        // See the force_path_style explanation in get_presigned_url.
        config_builder = config_builder.endpoint_url(ep).force_path_style(true);
    }

    let client = aws_sdk_s3::Client::from_conf(config_builder.build());

    let response = client
        .get_object()
        .bucket(bucket_name)
        .key(key)
        .send()
        .await?;

    let data = response.body.collect().await?.into_bytes();
    let text = String::from_utf8(data.to_vec())?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_s3_uri() {
        let uri = "s3://my-bucket/path/to/my-bundle.zip";
        let (bucket, key) = parse_s3_uri(uri).unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(key, "path/to/my-bundle.zip");
    }
}
