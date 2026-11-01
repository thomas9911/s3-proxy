import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { PutBucketPolicyCommand, S3Client } from "@aws-sdk/client-s3";

type OptionName =
	| "data-root"
	| "bucket"
	| "namespace"
	| "listen"
	| "external-url"
	| "access-key"
	| "secret-key"
	| "management-username"
	| "management-password";

const options = new Set<OptionName>([
	"data-root",
	"bucket",
	"namespace",
	"listen",
	"external-url",
	"access-key",
	"secret-key",
	"management-username",
	"management-password",
]);

function usage(): never {
	console.log(`Usage: bun run serve-data [options]

Options:
  --data-root <path>          Physical directory to expose (default: ./data)
  --bucket <name>             Virtual S3 bucket name (default: storage)
  --namespace <name>          Internal namespace (default: admin)
  --listen <host:port>        Bind address (default: 127.0.0.1:3000)
  --external-url <url>        Public proxy URL (default: http://127.0.0.1:3000)
  --access-key <key>          Initial S3 access key (default: admin)
  --secret-key <key>          Initial S3 secret key
  --public                    Configure the virtual bucket for public reads
  --help                      Show this help`);
	process.exit(0);
}

function parseArguments(): { settings: Record<OptionName, string>; makePublic: boolean } {
	const values: Partial<Record<OptionName, string>> = {};
	let makePublic = false;
	for (let index = 2; index < Bun.argv.length; index += 1) {
		const argument = Bun.argv[index];
		if (argument === "--help") usage();
		if (argument === "--public") {
			makePublic = true;
			continue;
		}
		if (!argument.startsWith("--")) usage();
		const name = argument.slice(2) as OptionName;
		const value = Bun.argv[++index];
		if (!options.has(name) || !value || value.startsWith("--")) usage();
		values[name] = value;
	}

	return {
		settings: {
			"data-root": values["data-root"] ?? resolve(import.meta.dir, "..", "data"),
			bucket: values.bucket ?? "storage",
			namespace: values.namespace ?? "admin",
			listen: values.listen ?? "127.0.0.1:3000",
			"external-url": values["external-url"] ?? "http://127.0.0.1:3000",
			"access-key": values["access-key"] ?? "admin",
			"secret-key": values["secret-key"] ?? "admin-secret-key-change-me",
			"management-username": values["management-username"] ?? "admin",
			"management-password": values["management-password"] ?? "management-password-change-me",
		},
		makePublic,
	};
}

const { settings, makePublic } = parseArguments();
const dataRoot = resolve(settings["data-root"]);

if (!existsSync(dataRoot)) {
	throw new Error(`Data directory does not exist: ${dataRoot}`);
}

const environment = {
	...process.env,
	S3_PROXY__SERVER_HOST: settings.listen,
	S3_PROXY__EXTERNAL_SERVER_HOST: settings["external-url"],
	S3_PROXY__METADATA_BACKEND: "sqlite",
	S3_PROXY__SQLITE__URL: "sqlite::memory:",
	S3_PROXY__OPENDAL_PROVIDER: "fs",
	S3_PROXY__OPENDAL__ROOT: dataRoot,
	S3_PROXY__STORAGE_LAYOUT: "single_bucket",
	S3_PROXY__SINGLE_BUCKET__NAMESPACE: settings.namespace,
	S3_PROXY__SINGLE_BUCKET__NAME: settings.bucket,
	S3_PROXY__ADMIN__ACCESS_KEY: settings["access-key"],
	S3_PROXY__ADMIN__SECRET_KEY: settings["secret-key"],
	S3_PROXY__MANAGEMENT__USERNAME: settings["management-username"],
	S3_PROXY__MANAGEMENT__PASSWORD: settings["management-password"],
};

console.log(`Serving ${dataRoot} as s3://${settings.bucket}/ at ${settings["external-url"]}`);
console.log(`Reserved proxy state is stored in ${dataRoot}\\.s3-proxy\\`);

const proxy = Bun.spawn(["cargo", "run", "--release", "--no-default-features"], {
	env: environment,
	stdin: "inherit",
	stdout: "inherit",
	stderr: "inherit",
});

if (makePublic) {
	for (let attempt = 0; attempt < 60; attempt += 1) {
		if (proxy.exitCode !== null) throw new Error("Proxy exited before becoming ready");
		try {
			const response = await fetch(`${settings["external-url"]}/healthz`);
			if (response.ok) break;
		} catch {
			// The proxy is still starting.
		}
		if (attempt === 59) throw new Error("Proxy did not become ready within 30 seconds");
		await Bun.sleep(500);
	}

	const s3 = new S3Client({
		endpoint: settings["external-url"],
		region: "us-east-1",
		forcePathStyle: true,
		credentials: {
			accessKeyId: settings["access-key"],
			secretAccessKey: settings["secret-key"],
		},
	});
	try {
		const policy = JSON.stringify({
			Version: "2012-10-17",
			Statement: [
				{
					Effect: "Allow",
					Principal: "*",
					Action: "s3:ListBucket",
					Resource: `arn:aws:s3:::${settings.bucket}`,
				},
				{
					Effect: "Allow",
					Principal: "*",
					Action: "s3:GetObject",
					Resource: `arn:aws:s3:::${settings.bucket}/*`,
				},
			],
		});
		await s3.send(new PutBucketPolicyCommand({ Bucket: settings.bucket, Policy: policy }));
		console.log(`Configured s3://${settings.bucket}/ for public reads`);
	} finally {
		s3.destroy();
	}
}

process.exitCode = await proxy.exited;
