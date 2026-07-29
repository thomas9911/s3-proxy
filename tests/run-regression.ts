const baseEnv = {
	...process.env,
	S3_TEST_ACCESS_KEY: process.env.S3_TEST_ACCESS_KEY ?? "minioadmin",
	S3_TEST_SECRET_KEY: process.env.S3_TEST_SECRET_KEY ?? "minioadmin",
	S3_TEST_REGION: process.env.S3_TEST_REGION ?? "us-east-1",
	S3_TEST_PROFILE: process.env.S3_TEST_PROFILE ?? "1",
};

function run(label: string, variables: Record<string, string>) {
	console.log(`\n[s3-regression-wrapper] running ${label}`);
	const result = Bun.spawnSync(["bun", "test", "tests/s3-regression.test.ts"], {
		env: { ...baseEnv, ...variables },
		stdout: "inherit",
		stderr: "inherit",
	});
	if (result.exitCode !== 0) {
		throw new Error(`${label} regression checks failed`);
	}
}

const externalEndpoint =
	process.env.S3_TEST_EXTERNAL_ENDPOINT ?? process.env.S3_TEST_ENDPOINT;

if (externalEndpoint) {
	run("external", {
		S3_TEST_TARGET: "external",
		S3_TEST_ENDPOINT: externalEndpoint,
	});
} else {
	run("MinIO in Docker", {
		S3_TEST_TARGET: "minio",
		S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	});
}

run("proxy with Redis metadata", {
	S3_TEST_TARGET: "proxy",
	S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	S3_TEST_METADATA_BACKEND: "redis",
	S3_TEST_OPENDAL_PROVIDER: "memory",
});

run("proxy with SQLite metadata", {
	S3_TEST_TARGET: "proxy",
	S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	S3_TEST_METADATA_BACKEND: "sqlite",
	S3_TEST_OPENDAL_PROVIDER: "memory",
});

run("proxy with FS", {
	S3_TEST_TARGET: "proxy",
	S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	S3_TEST_METADATA_BACKEND: "sqlite",
	S3_TEST_OPENDAL_PROVIDER: "fs",
});

run("proxy with Sled", {
	S3_TEST_TARGET: "proxy",
	S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	S3_TEST_METADATA_BACKEND: "sqlite",
	S3_TEST_OPENDAL_PROVIDER: "sled",
	S3_TEST_BATCH_SIZE: "50",
});

run("proxy with PostgreSQL metadata", {
	S3_TEST_TARGET: "proxy",
	S3_TEST_ENDPOINT: "http://127.0.0.1:19000",
	S3_TEST_METADATA_BACKEND: "postgres",
	S3_TEST_OPENDAL_PROVIDER: "memory",
});
