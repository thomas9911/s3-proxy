use opendal::Operator;
use std::collections::HashMap;

pub fn probe() {
    let schemes = [
        "azblob",
        "b2",
        "cloudflare-kv",
        "compfs",
        "cos",
        "dashmap",
        "fs",
        "gcs",
        "github",
        "koofr",
        "memory",
        "mini-moka",
        "moka",
        "mysql",
        "obs",
        "oss",
        "s3",
        "seafile",
        "sled",
        "swift",
        "tos",
        "upyun",
        "vercel-blob",
        "webdav",
        "yandex-disk",
    ];
    let default_options = HashMap::from([
        ("root".to_string(), "/tmp".to_string()),
        ("container".to_string(), "tmp".to_string()),
        ("filesystem".to_string(), "tmp".to_string()),
        ("bucket".to_string(), "tmp".to_string()),
        ("bucket_id".to_string(), "tmp-bucket-id".to_string()),
        (
            "application_key_id".to_string(),
            "tmp-application-key-id".to_string(),
        ),
        (
            "application_key".to_string(),
            "tmp-application-key".to_string(),
        ),
        ("repo_name".to_string(), "tmp-repo".to_string()),
        ("table".to_string(), "tmp-table".to_string()),
        ("key_field".to_string(), "key".to_string()),
        ("value_field".to_string(), "value".to_string()),
        (
            "datadir".to_string(),
            "target/opendal-backend-data".to_string(),
        ),
        (
            "datafile".to_string(),
            "target/opendal-backend.data".to_string(),
        ),
        (
            "connection_string".to_string(),
            "sqlite://target/opendal-backend.db".to_string(),
        ),
        ("region".to_string(), "us-east-1".to_string()),
        ("endpoint".to_string(), "http://127.0.0.1:9000".to_string()),
        ("account_name".to_string(), "abc".to_string()),
        (
            "account_key".to_string(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string(),
        ),
        ("access_key_id".to_string(), "abc".to_string()),
        ("secret_access_key".to_string(), "abc".to_string()),
        ("access_token".to_string(), "abc".to_string()),
        ("api_token".to_string(), "abc".to_string()),
        ("token".to_string(), "abc".to_string()),
        ("operator".to_string(), "tmp-operator".to_string()),
        ("username".to_string(), "abc".to_string()),
        ("password".to_string(), "abc".to_string()),
    ]);

    for scheme in schemes {
        let mut options = if scheme == "memory" {
            HashMap::new()
        } else {
            default_options.clone()
        };

        if scheme == "sled" {
            options.insert(
                "datadir".to_string(),
                "target/opendal-sled-backend-data".to_string(),
            );
        }

        let operator = match scheme {
            // These services take required identifiers from URI authority/path.
            "cloudflare-kv" => {
                Operator::from_uri(("cloudflare-kv://tmp-account/tmp-namespace", options))
            }
            "github" => Operator::from_uri(("github://tmp-owner/tmp-repo", options)),
            "koofr" => Operator::from_uri(("koofr://api.koofr.net/test%40example.com", options)),
            _ => Operator::via_iter(scheme, options),
        };
        match operator {
            Ok(operator) => {
                let capability = operator.info().full_capability();
                println!(
                    "{scheme} => proxy_compatibility={} {:?}",
                    proxy_compatibility(&capability),
                    capability
                );
            }
            Err(error) => println!("{scheme} => unavailable: {error}"),
        }
    }
}

fn proxy_compatibility(capability: &opendal::Capability) -> &'static str {
    if supports_proxy(capability) {
        "full"
    } else if supports_partial_proxy(capability) {
        "partial"
    } else {
        "none"
    }
}

fn supports_proxy(capability: &opendal::Capability) -> bool {
    capability.stat
        && capability.read
        && capability.write
        && capability.write_can_empty
        && capability.write_with_content_type
        && capability.create_dir
        && capability.delete
        && capability.list
        && capability.list_with_recursive
}

fn supports_partial_proxy(capability: &opendal::Capability) -> bool {
    capability.stat
        && capability.read
        && capability.write
        && capability.write_can_empty
        && capability.create_dir
        && capability.delete
        && capability.list
}
