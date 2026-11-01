use std::collections::HashMap;

use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Bucket, CompletedPart, Delete, ObjectIdentifier, Owner};
use aws_sdk_s3::Client;
use s3_proxy::metadata::MetaDataBackend;
#[cfg(feature = "management")]
use s3_proxy::ManagementConfig;
use s3_proxy::{build_app, AdminConfig, AppState, Config, SqliteConfig};

