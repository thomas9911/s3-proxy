import { Database } from "bun:sqlite";
import { SQL, S3Client as BunS3Client } from "bun";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { unlink } from "node:fs/promises";
import { Sha256 } from "@smithy/core/checksum";
import { HttpRequest } from "@smithy/core/protocols";
import { SignatureV4 } from "@smithy/signature-v4";
import {
	AbortMultipartUploadCommand,
	CreateBucketCommand,
	CopyObjectCommand,
	CompleteMultipartUploadCommand,
	CreateMultipartUploadCommand,
	DeleteBucketCommand,
	DeleteObjectCommand,
	DeleteObjectsCommand,
	DeleteBucketPolicyCommand,
	GetObjectCommand,
	GetBucketPolicyCommand,
	GetBucketVersioningCommand,
	HeadObjectCommand,
	ListBucketsCommand,
	ListObjectVersionsCommand,
	ListPartsCommand,
	ListObjectsV2Command,
	PutObjectCommand,
	PutBucketPolicyCommand,
	PutBucketVersioningCommand,
	UploadPartCommand,
	S3Client,
} from "@aws-sdk/client-s3";
import { createPresignedPost } from "@aws-sdk/s3-presigned-post";

const target =
	process.env.S3_TEST_TARGET ??
	(process.env.S3_TEST_ENDPOINT ? "external" : "minio");
const metadataBackend = process.env.S3_TEST_METADATA_BACKEND ?? "redis";
const opendalProvider = process.env.S3_TEST_OPENDAL_PROVIDER ?? "memory";
const endpoint = process.env.S3_TEST_ENDPOINT ?? "http://127.0.0.1:19000";
const accessKeyId = process.env.S3_TEST_ACCESS_KEY ?? "minioadmin";
const secretAccessKey = process.env.S3_TEST_SECRET_KEY ?? "minioadmin";
const region = process.env.S3_TEST_REGION ?? "us-east-1";
const profileRegression = process.env.S3_TEST_PROFILE === "1";
const configuredBatchSize = Number.parseInt(
	process.env.S3_TEST_BATCH_SIZE ?? "200",
	10,
);
const batchSize = Number.isFinite(configuredBatchSize)
	? Math.max(1, configuredBatchSize)
	: 200;
const bucket = `regression-${Date.now().toString(36)}`;
const objectKey = "nested/hello.txt";
const objectBody = "s3-proxy regression test\n";
const containerName = `s3-proxy-regression-${process.pid}`;
const redisContainerName = `s3-proxy-regression-redis-${process.pid}`;
const postgresContainerName = `s3-proxy-regression-postgres-${process.pid}`;
const postgresDatabaseUrl = "postgres://postgres:postgres@127.0.0.1:15432/s3proxy";
const sqliteDatabasePath = `${process.cwd()}\\target\\s3-regression-${process.pid}.db`;
const sqliteDatabaseUrl =
	process.env.S3_TEST_SQLITE_URL ??
	`sqlite://target/s3-regression-${process.pid}.db`;
const ownsSqliteDatabase = !process.env.S3_TEST_SQLITE_URL;
const rcloneBinary = process.env.RCLONE ?? `${process.cwd()}\\rclone.exe`;
const managementUsername = "dashboard";
const managementPassword = "dashboard-secret";
let ownsContainer = false;
let proxyProcess: Bun.Subprocess | undefined;

function run(command: string[], options: { check?: boolean } = {}) {
	const result = Bun.spawnSync(command, { stdout: "pipe", stderr: "pipe" });
	const stdout = new TextDecoder().decode(result.stdout).trim();
	const stderr = new TextDecoder().decode(result.stderr).trim();
	if (options.check !== false && result.exitCode !== 0) {
		throw new Error(
			`${command.join(" ")} failed (${result.exitCode}): ${stderr}`,
		);
	}
	return { stdout, stderr, exitCode: result.exitCode };
}

function sleep(milliseconds: number) {
	return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function expectS3Error<T>(
	operation: Promise<T>,
	status: number,
	names: string[],
) {
	let error: unknown;
	try {
		await operation;
	} catch (caught) {
		error = caught;
	}
	if (!error)
		throw new Error(`expected S3 request to fail with HTTP ${status}`);
	const response = error as {
		name?: string;
		$metadata?: { httpStatusCode?: number };
	};
	expect(response.$metadata?.httpStatusCode).toBe(status);
	expect(names).toContain(response.name ?? "");
}

function rcloneConfig() {
	return [
		"[regression]",
		"type = s3",
		"provider = Minio",
		`access_key_id = ${accessKeyId}`,
		`secret_access_key = ${secretAccessKey}`,
		`endpoint = ${endpoint}`,
		`region = ${region}`,
		"force_path_style = true",
		"",
	].join("\n");
}

async function rclone(args: string[]) {
	const config = `${import.meta.dir}/.rclone-regression.conf`;
	await Bun.write(config, rcloneConfig());
	return run([rcloneBinary, "--config", config, ...args]);
}

async function signedDelete(body: string, contentMd5?: string) {
	const targetUrl = new URL(`${endpoint}/${bucket}?delete`);
	const amzDate = new Date()
		.toISOString()
		.replace(/[-:]/g, "")
		.replace(/\.\d{3}Z$/, "Z");
	const bodyHash = new Bun.CryptoHasher("sha256")
		.update(body)
		.digest("hex");
	const request = new HttpRequest({
		method: "POST",
		protocol: targetUrl.protocol,
		hostname: targetUrl.hostname,
		port: targetUrl.port ? Number(targetUrl.port) : undefined,
		path: targetUrl.pathname,
		query: { delete: "" },
		headers: {
			host: targetUrl.host,
			"content-type": "application/xml",
			"x-amz-content-sha256": bodyHash,
			"x-amz-date": amzDate,
			...(contentMd5 ? { "content-md5": contentMd5 } : {}),
		},
		body,
	});
	const signer = new SignatureV4({
		credentials: { accessKeyId, secretAccessKey },
		region,
		service: "s3",
		sha256: Sha256,
		uriEscapePath: false,
	});
	const signed = await signer.sign(request);
	return fetch(targetUrl, {
		method: "POST",
		headers: signed.headers,
		body,
	});
}

async function signedIamAction(parameters: Record<string, string>) {
	const targetUrl = new URL(endpoint);
	const body = new URLSearchParams({
		Version: "2010-05-08",
		...parameters,
	}).toString();
	const amzDate = new Date()
		.toISOString()
		.replace(/[-:]/g, "")
		.replace(/\.\d{3}Z$/, "Z");
	const bodyHash = new Bun.CryptoHasher("sha256").update(body).digest("hex");
	const request = new HttpRequest({
		method: "POST",
		protocol: targetUrl.protocol,
		hostname: targetUrl.hostname,
		port: targetUrl.port ? Number(targetUrl.port) : undefined,
		path: targetUrl.pathname || "/",
		headers: {
			host: targetUrl.host,
			"content-type": "application/x-www-form-urlencoded",
			"x-amz-content-sha256": bodyHash,
			"x-amz-date": amzDate,
		},
		body,
	});
	const signer = new SignatureV4({
		credentials: { accessKeyId, secretAccessKey },
		region,
		service: "s3",
		sha256: Sha256,
		uriEscapePath: false,
	});
	const signed = await signer.sign(request);
	return fetch(targetUrl, { method: "POST", headers: signed.headers, body });
}

function signedCreateAccessKey(userName: string) {
	return signedIamAction({ Action: "CreateAccessKey", UserName: userName });
}

function signedDeleteAccessKey(accessKeyId: string, userName: string) {
	return signedIamAction({
		Action: "DeleteAccessKey",
		AccessKeyId: accessKeyId,
		UserName: userName,
	});
}

function signedListAccessKeys(userName: string) {
	return signedIamAction({ Action: "ListAccessKeys", UserName: userName });
}

function signedUpdateAccessKey(accessKeyId: string, status: "Active" | "Inactive") {
	return signedIamAction({ Action: "UpdateAccessKey", AccessKeyId: accessKeyId, Status: status });
}

function signedGetAccessKeyLastUsed(accessKeyId: string) {
	return signedIamAction({ Action: "GetAccessKeyLastUsed", AccessKeyId: accessKeyId });
}

async function waitForEndpoint() {
	for (let attempt = 0; attempt < 60; attempt++) {
		try {
			await fetch(`${endpoint}/`);
			return;
		} catch {
			await sleep(500);
		}
	}
	throw new Error(`${target} did not become ready at ${endpoint}`);
}

async function startTarget() {
	if (target === "external") {
		if (!process.env.S3_TEST_ENDPOINT)
			throw new Error(
				"S3_TEST_ENDPOINT is required for S3_TEST_TARGET=external",
			);
		return;
	}

	if (target === "proxy") {
		if (metadataBackend === "redis") {
			run(["docker", "rm", "-f", redisContainerName], { check: false });
			run([
				"docker",
				"run",
				"--detach",
				"--name",
				redisContainerName,
				"--publish",
				"16379:6379",
				"redis:latest",
			]);
			for (let attempt = 0; attempt < 30; attempt++) {
				const seeded = run(
					[
						"docker",
						"exec",
						redisContainerName,
						"redis-cli",
						"set",
						`secret_key::${accessKeyId}`,
						secretAccessKey,
					],
					{ check: false },
				);
				if (seeded.exitCode === 0) break;
				if (attempt === 29) throw new Error("Redis did not become ready");
				await sleep(500);
			}
		} else if (metadataBackend === "postgres") {
			run(["docker", "rm", "-f", postgresContainerName], { check: false });
			run([
				"docker",
				"run",
				"--detach",
				"--name",
				postgresContainerName,
				"--publish",
				"15432:5432",
				"--env",
				"POSTGRES_PASSWORD=postgres",
				"--env",
				"POSTGRES_DB=s3proxy",
				"postgres:latest",
			]);
			let seeded = false;
			for (let attempt = 0; attempt < 30; attempt++) {
				const postgres = new SQL(postgresDatabaseUrl);
				try {
					await postgres`SELECT 1`;
					await postgres`
						CREATE TABLE IF NOT EXISTS access_keys (
							access_key TEXT PRIMARY KEY,
							secret_key TEXT NOT NULL
						)
					`;
					await postgres`
						INSERT INTO access_keys (access_key, secret_key)
						VALUES (${accessKeyId}, ${secretAccessKey})
						ON CONFLICT (access_key)
						DO UPDATE SET secret_key = EXCLUDED.secret_key
					`;
					await postgres.close();
					seeded = true;
					break;
				} catch (error) {
					await postgres.close({ timeout: 0 }).catch(() => {});
					if (attempt === 29) throw error;
					await sleep(500);
				}
			}
			if (!seeded) throw new Error("PostgreSQL did not become ready");
		} else if (metadataBackend !== "sqlite") {
			throw new Error(`unknown metadata backend: ${metadataBackend}`);
		} else {
			const database = new Database(sqliteDatabasePath);
			database.run(`
				CREATE TABLE IF NOT EXISTS access_keys (
					access_key TEXT PRIMARY KEY,
					secret_key TEXT NOT NULL
				)
			`);
			database.run(
				`INSERT INTO access_keys (access_key, secret_key) VALUES (?, ?)
				 ON CONFLICT(access_key) DO UPDATE SET secret_key = excluded.secret_key`,
				[accessKeyId, secretAccessKey],
			);
			database.close();
    }
    const cargoRunArgs = ["cargo", "run", "--quiet"];
    cargoRunArgs.push("--release");
		proxyProcess = Bun.spawn(cargoRunArgs, {
				env: {
					...process.env,
					S3_PROXY__SERVER_HOST: "0.0.0.0:19000",
					S3_PROXY__EXTERNAL_SERVER_HOST: endpoint,
					S3_PROXY__ADMIN__ACCESS_KEY: accessKeyId,
					S3_PROXY__ADMIN__SECRET_KEY: secretAccessKey,
					S3_PROXY__MANAGEMENT__USERNAME: managementUsername,
					S3_PROXY__MANAGEMENT__PASSWORD: managementPassword,
				S3_PROXY__METADATA_BACKEND: metadataBackend,
				...(metadataBackend === "redis"
					? { S3_PROXY__REDIS__URL: "redis://127.0.0.1:16379" }
					: metadataBackend === "postgres"
						? { S3_PROXY__POSTGRES__URL: postgresDatabaseUrl }
						: { S3_PROXY__SQLITE__URL: sqliteDatabaseUrl }),
				S3_PROXY__OPENDAL_PROVIDER: opendalProvider,
				S3_PROXY__OPENDAL__ROOT: "/tmp",
				...(opendalProvider === "sled"
					? {
						S3_PROXY__OPENDAL__DATADIR: `target/s3-regression-sled-${process.pid}`,
					}
					: {}),
			},
			stdout: "ignore",
			stderr: "pipe",
		});
		await waitForEndpoint();
		return;
	}

	if (target !== "minio") throw new Error(`unknown S3_TEST_TARGET: ${target}`);
	run(["docker", "rm", "-f", containerName], { check: false });
	run([
		"docker",
		"run",
		"--detach",
		"--name",
		containerName,
		"--publish",
		"19000:9000",
		"--env",
		`MINIO_ROOT_USER=${accessKeyId}`,
		"--env",
		`MINIO_ROOT_PASSWORD=${secretAccessKey}`,
		"minio/minio:latest",
		"server",
		"/data",
	]);
	ownsContainer = true;
	await waitForEndpoint();
}

function reportTargetVersion() {
	if (target === "minio") {
		const version = run(
			["docker", "exec", containerName, "minio", "--version"],
			{ check: false },
		);
		const release =
			version.stdout.match(/minio version\s+([^\s]+)/i)?.[1] ?? "unknown";
		console.log(`[s3-regression] target=minio version=${release}`);
		return;
	}
	if (target === "proxy") {
		const metadata = JSON.parse(
			run(["cargo", "metadata", "--no-deps", "--format-version", "1"]).stdout,
		);
		const packageInfo = metadata.packages.find(
			(item: { name: string }) => item.name === "s3-proxy",
		);
		console.log(
			`[s3-regression] target=proxy version=${packageInfo?.version ?? "unknown"}`,
		);
		return;
	}
	console.log(
		`[s3-regression] target=external version=${process.env.S3_TEST_VERSION ?? "unknown"}`,
	);
}

function client() {
	return new S3Client({
		endpoint,
		region,
		forcePathStyle: true,
		credentials: { accessKeyId, secretAccessKey },
	});
}

async function presignedGetUrl(key: string) {
	const signer = new BunS3Client({
		accessKeyId,
		secretAccessKey,
		bucket,
		endpoint,
		region,
	});
	return signer.presign(key, { expiresIn: 604_800 });
}

let s3: S3Client;
beforeAll(async () => {
	await startTarget();
	reportTargetVersion();
	s3 = client();
}, 60_000);
afterAll(async () => {
	s3?.destroy();
	proxyProcess?.kill();
	if (ownsContainer)
		run(["docker", "rm", "--force", containerName], { check: false });
	if (target === "proxy" && metadataBackend === "redis")
		run(["docker", "rm", "--force", redisContainerName], { check: false });
	if (target === "proxy" && metadataBackend === "postgres")
		run(["docker", "rm", "--force", postgresContainerName], { check: false });
	if (target === "proxy" && ownsSqliteDatabase)
		await unlink(sqliteDatabasePath).catch(() => {});
});

describe("S3 compatibility contract", () => {
	test("exposes health, readiness, and metrics endpoints", async () => {
		if (target !== "proxy") return;
		const health = await fetch(`${endpoint}/healthz`);
		expect(health.status).toBe(200);
		expect((await health.json()).status).toBe("ok");
		const ready = await fetch(`${endpoint}/readyz`);
		expect(ready.status).toBe(200);
		expect((await ready.json()).status).toBe("ready");
		const metrics = await fetch(`${endpoint}/metrics`);
		expect(metrics.status).toBe(200);
		expect(await metrics.text()).toContain("s3_proxy_http_requests_total");
	});

	test("exposes a separately authenticated management dashboard", async () => {
		if (target !== "proxy") return;
		const authorization = `Basic ${btoa(`${managementUsername}:${managementPassword}`)}`;
		const denied = await fetch(`${endpoint}/admin`);
		expect(denied.status).toBe(401);
		expect(denied.headers.get("www-authenticate")).toContain("Basic");
		const dashboard = await fetch(`${endpoint}/admin`, {
			headers: { authorization },
		});
		expect(dashboard.status).toBe(200);
		expect(await dashboard.text()).toContain("/admin/fragments/buckets");
		const status = await fetch(`${endpoint}/admin/api/status`, {
			headers: { authorization },
		});
		expect(status.status).toBe(200);
		const statusData = (await status.json()) as {
			metadata_ready: boolean;
			metadata_backend: string;
			opendal_provider: string;
			storage_capabilities: string[];
		};
		expect(statusData.metadata_ready).toBe(true);
		expect(statusData.metadata_backend).toBe("sqlite");
		expect(statusData.opendal_provider).toBe("memory");
		expect(statusData.storage_capabilities).toContain("recursive_list");
		const audit = await fetch(`${endpoint}/admin/api/audit?limit=5`, {
			headers: { authorization },
		});
		expect(audit.status).toBe(200);
		expect(
			((await audit.json()) as Array<{ path: string }>).some(
				(event) => event.path === "/admin/api/status",
			),
		).toBe(true);
		const managementMetrics = await fetch(`${endpoint}/admin/api/metrics`, {
			headers: { authorization },
		});
		expect(managementMetrics.status).toBe(200);
		expect(await managementMetrics.text()).toContain(
			"s3_proxy_http_requests_total",
		);
		const principals = await fetch(`${endpoint}/admin/api/principals`, {
			headers: { authorization },
		});
		expect(principals.status).toBe(200);
		const data = (await principals.json()) as Array<{
			namespace: string;
			access_keys: Array<Record<string, string>>;
		}>;
		const principal = data.find((entry) => entry.namespace === accessKeyId);
		expect(principal).toBeDefined();
		expect(principal?.access_keys[0]?.id).toBe(accessKeyId);
		expect(principal?.access_keys[0]?.secret_key).toBeUndefined();
		const managementBucket = `management-${Date.now().toString(36)}`;
		const created = await fetch(`${endpoint}/admin/api/buckets`, {
			method: "POST",
			headers: {
				authorization,
				"content-type": "application/x-www-form-urlencoded",
			},
			body: new URLSearchParams({
				namespace: accessKeyId,
				name: managementBucket,
			}),
		});
		expect(created.status).toBe(201);
		try {
			const policy = JSON.stringify({
				Version: "2012-10-17",
				Statement: [],
			});
			const configuration = await fetch(
				`${endpoint}/admin/api/bucket-configuration`,
				{
					method: "POST",
					headers: {
						authorization,
						"content-type": "application/x-www-form-urlencoded",
					},
					body: new URLSearchParams({
						namespace: accessKeyId,
						bucket: managementBucket,
						versioning: "Enabled",
						policy,
					}),
				},
			);
			expect(configuration.status).toBe(204);
			const configured = await fetch(
				`${endpoint}/admin/api/inspect?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
				})}`,
				{ headers: { authorization } },
			);
			expect(configured.status).toBe(200);
			const configuredData = (await configured.json()) as {
				versioning: string;
				bucket_policy: string | null;
			};
			expect(configuredData.versioning).toBe("Enabled");
			expect(configuredData.bucket_policy).toBe(policy);
			const upload = new FormData();
			upload.set("namespace", accessKeyId);
			upload.set("bucket", managementBucket);
			upload.set("key", "dashboard-upload.txt");
			upload.set(
				"file",
				new Blob(["uploaded through management dashboard"], {
					type: "text/plain",
				}),
				"dashboard-upload.txt",
			);
			const uploaded = await fetch(`${endpoint}/admin/api/objects`, {
				method: "POST",
				headers: { authorization },
				body: upload,
			});
			expect(uploaded.status).toBe(200);
			const listedObjects = await fetch(
				`${endpoint}/admin/api/objects?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
				})}`,
				{ headers: { authorization } },
			);
			expect(listedObjects.status).toBe(200);
			expect(
				((await listedObjects.json()) as Array<{ key: string }>).some(
					(object) => object.key === "dashboard-upload.txt",
				),
			).toBe(true);
			const uploadedObject = await s3.send(
				new GetObjectCommand({
					Bucket: managementBucket,
					Key: "dashboard-upload.txt",
				}),
			);
			expect(await uploadedObject.Body?.transformToString()).toBe(
				"uploaded through management dashboard",
			);
			const presigned = await fetch(`${endpoint}/admin/api/presigned-urls`, {
				method: "POST",
				headers: {
					authorization,
					"content-type": "application/x-www-form-urlencoded",
				},
				body: new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					key: "dashboard-upload.txt",
					access_key: accessKeyId,
					expires_in: "60",
				}),
			});
			expect(presigned.status).toBe(200);
			const presignedData = (await presigned.json()) as { url: string };
			expect(presignedData.url).toContain("X-Amz-Signature=");
			const presignedObject = await fetch(presignedData.url);
			expect(presignedObject.status).toBe(200);
			expect(await presignedObject.text()).toBe(
				"uploaded through management dashboard",
			);
			await s3.send(
				new PutObjectCommand({
					Bucket: managementBucket,
					Key: "dashboard-metadata.txt",
					Body: "metadata inspection",
					Metadata: { dashboard: "visible" },
				}),
			);
			const inspection = await fetch(
				`${endpoint}/admin/api/inspect?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					key: "dashboard-metadata.txt",
				})}`,
				{ headers: { authorization } },
			);
			expect(inspection.status).toBe(200);
			const inspectionData = (await inspection.json()) as {
				bucket_public: boolean;
				object_public: boolean;
				bucket_policy: string | null;
				versioning: string;
				metadata: Array<{ key: string; value: string }>;
			};
			expect(inspectionData.bucket_public).toBe(false);
			expect(inspectionData.object_public).toBe(false);
			expect(inspectionData.bucket_policy).toBe(policy);
			expect(inspectionData.versioning).toBe("Enabled");
			expect(
				inspectionData.metadata.some(
					(entry) => entry.key === "dashboard" && entry.value === "visible",
			),
		).toBe(true);
			const inspectionFragment = await fetch(
				`${endpoint}/admin/fragments/inspect?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					key: "dashboard-metadata.txt",
				})}`,
				{ headers: { authorization } },
			);
			expect(inspectionFragment.status).toBe(200);
			expect(await inspectionFragment.text()).toContain("Object metadata");
			const download = await fetch(
				`${endpoint}/admin/api/download?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					key: "dashboard-upload.txt",
				})}`,
				{ headers: { authorization } },
			);
			expect(download.status).toBe(200);
			expect(download.headers.get("content-disposition")).toContain(
				"attachment",
			);
			expect(await download.text()).toBe("uploaded through management dashboard");
			const presignedPost = await fetch(
				`${endpoint}/admin/api/presigned-posts`,
				{
					method: "POST",
					headers: {
						authorization,
						"content-type": "application/x-www-form-urlencoded",
					},
					body: new URLSearchParams({
						namespace: accessKeyId,
						bucket: managementBucket,
						access_key: accessKeyId,
						prefix: "dashboard-post/",
						expires_in: "60",
						max_size: "1024",
					}),
				},
			);
			expect(presignedPost.status).toBe(200);
			const postData = (await presignedPost.json()) as {
				url: string;
				fields: Record<string, string>;
			};
			const postForm = new FormData();
			for (const [name, value] of Object.entries(postData.fields)) {
				postForm.set(name, value);
			}
			postForm.set(
				"file",
				new Blob(["uploaded from generated post form"], { type: "text/plain" }),
				"dashboard.txt",
			);
			const postUpload = await fetch(postData.url, {
				method: "POST",
				body: postForm,
			});
			expect(postUpload.status).toBe(204);
			const postObject = await s3.send(
				new GetObjectCommand({
					Bucket: managementBucket,
					Key: "dashboard-post/dashboard.txt",
				}),
			);
			expect(await postObject.Body?.transformToString()).toBe(
				"uploaded from generated post form",
			);
			const buckets = await fetch(`${endpoint}/admin/api/buckets`, {
				headers: { authorization },
			});
			expect(buckets.status).toBe(200);
			const bucketData = (await buckets.json()) as Array<{ name: string }>;
			expect(bucketData.some((entry) => entry.name === managementBucket)).toBe(
				true,
			);
			const filteredBuckets = await fetch(
				`${endpoint}/admin/api/buckets?${new URLSearchParams({
					prefix: managementBucket,
				})}`,
				{ headers: { authorization } },
			);
			expect(filteredBuckets.status).toBe(200);
			expect(
				((await filteredBuckets.json()) as Array<{ name: string }>).every(
					(entry) => entry.name.startsWith(managementBucket),
				),
			).toBe(true);
			const fragment = await fetch(`${endpoint}/admin/fragments/buckets`, {
				headers: { authorization },
			});
			expect(fragment.status).toBe(200);
			expect(await fragment.text()).toContain(managementBucket);
		} finally {
			await s3
				.send(
					new DeleteObjectCommand({
						Bucket: managementBucket,
						Key: "dashboard-upload.txt",
					}),
				)
				.catch(() => {});
			await s3.send(new DeleteBucketCommand({ Bucket: managementBucket }));
		}
	});

	test("requires confirmation for management object and bucket deletion", async () => {
		if (target !== "proxy") return;
		const authorization = `Basic ${btoa(`${managementUsername}:${managementPassword}`)}`;
		const managementBucket = `management-delete-${Date.now().toString(36)}`;
		await s3.send(new CreateBucketCommand({ Bucket: managementBucket }));
		await s3.send(
			new PutObjectCommand({
				Bucket: managementBucket,
				Key: "delete-me.txt",
				Body: "delete me",
			}),
		);
		const deleteObject = (confirm: boolean) =>
			fetch(`${endpoint}/admin/api/objects`, {
				method: "DELETE",
				headers: {
					authorization,
					"content-type": "application/x-www-form-urlencoded",
				},
				body: new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					key: "delete-me.txt",
					confirm: String(confirm),
				}),
			});
		expect((await deleteObject(false)).status).toBe(400);
		expect((await deleteObject(true)).status).toBe(204);
		const objectIsMissing = await s3
			.send(
				new GetObjectCommand({
					Bucket: managementBucket,
					Key: "delete-me.txt",
				}),
			)
			.then(
				() => false,
				() => true,
			);
		expect(objectIsMissing).toBe(true);
		const deleteBucket = (confirm: boolean) =>
			fetch(`${endpoint}/admin/api/buckets`, {
				method: "DELETE",
				headers: {
					authorization,
					"content-type": "application/x-www-form-urlencoded",
				},
				body: new URLSearchParams({
					namespace: accessKeyId,
					name: managementBucket,
					confirm: String(confirm),
				}),
			});
		expect((await deleteBucket(false)).status).toBe(400);
		expect((await deleteBucket(true)).status).toBe(204);
	});

	test("manages access keys from the dashboard", async () => {
		if (target !== "proxy") return;
		const authorization = `Basic ${btoa(`${managementUsername}:${managementPassword}`)}`;
		const formRequest = (
			url: string,
			method: "POST" | "PATCH" | "DELETE",
			body: Record<string, string>,
		) =>
			fetch(`${endpoint}${url}`, {
				method,
				headers: {
					authorization,
					"content-type": "application/x-www-form-urlencoded",
				},
				body: new URLSearchParams(body),
			});
		const created = await formRequest("/admin/api/access-keys", "POST", {
			namespace: accessKeyId,
		});
		expect(created.status).toBe(201);
		const original = (await created.json()) as {
			id: string;
			secret_key: string;
		};
		expect(original.id).toStartWith("AKIA");
		expect(original.secret_key.length).toBeGreaterThan(20);
		try {
			expect(
				(
					await formRequest("/admin/api/access-keys", "PATCH", {
						namespace: accessKeyId,
						access_key: original.id,
						status: "Inactive",
					})
				).status,
			).toBe(204);
			expect(
				(
					await formRequest("/admin/api/access-keys", "PATCH", {
						namespace: accessKeyId,
						access_key: original.id,
						status: "Active",
					})
				).status,
			).toBe(204);
			const rotated = await formRequest("/admin/api/access-keys/rotate", "POST", {
				namespace: accessKeyId,
				access_key: original.id,
			});
			expect(rotated.status).toBe(201);
			const replacement = (await rotated.json()) as {
				id: string;
				secret_key: string;
			};
			expect(replacement.id).not.toBe(original.id);
			expect(replacement.secret_key.length).toBeGreaterThan(20);
			const principals = await fetch(`${endpoint}/admin/api/principals`, {
				headers: { authorization },
			});
			const principal = (
				(await principals.json()) as Array<{
					namespace: string;
					access_keys: Array<{ id: string; status: string }>;
				}>
			).find((entry) => entry.namespace === accessKeyId);
			expect(
				principal?.access_keys.find((key) => key.id === original.id)?.status,
			).toBe("Inactive");
			expect(
				principal?.access_keys.find((key) => key.id === replacement.id)?.status,
			).toBe("Active");
			expect(
				(
					await formRequest("/admin/api/access-keys", "DELETE", {
						namespace: accessKeyId,
						access_key: replacement.id,
						confirm: "false",
					})
				).status,
			).toBe(400);
			expect(
				(
					await formRequest("/admin/api/access-keys", "DELETE", {
						namespace: accessKeyId,
						access_key: replacement.id,
						confirm: "true",
					})
				).status,
			).toBe(204);
		} finally {
			await formRequest("/admin/api/access-keys", "DELETE", {
				namespace: accessKeyId,
				access_key: original.id,
				confirm: "true",
			}).catch(() => {});
		}
	});

	test("lists and aborts multipart uploads from the dashboard", async () => {
		if (target !== "proxy") return;
		const authorization = `Basic ${btoa(`${managementUsername}:${managementPassword}`)}`;
		const managementBucket = `management-multipart-${Date.now().toString(36)}`;
		await s3.send(new CreateBucketCommand({ Bucket: managementBucket }));
		try {
			const initiated = await s3.send(
				new CreateMultipartUploadCommand({
					Bucket: managementBucket,
					Key: "pending/upload.txt",
				}),
			);
			const uploadId = initiated.UploadId;
			expect(uploadId).toBeDefined();
			const uploads = await fetch(
				`${endpoint}/admin/api/multipart-uploads?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
				})}`,
				{ headers: { authorization } },
			);
			expect(uploads.status).toBe(200);
			expect(
				(
					(await uploads.json()) as Array<{ upload_id: string; key: string }>
				).some(
					(upload) =>
						upload.upload_id === uploadId && upload.key === "pending/upload.txt",
				),
			).toBe(true);
			const abort = (confirm: boolean) =>
				fetch(`${endpoint}/admin/api/multipart-uploads`, {
					method: "DELETE",
					headers: {
						authorization,
						"content-type": "application/x-www-form-urlencoded",
					},
					body: new URLSearchParams({
						namespace: accessKeyId,
						bucket: managementBucket,
						upload_id: uploadId ?? "",
						confirm: String(confirm),
					}),
				});
			expect((await abort(false)).status).toBe(400);
			expect((await abort(true)).status).toBe(204);
			await s3.send(
				new CreateMultipartUploadCommand({
					Bucket: managementBucket,
					Key: "pending/stale.txt",
				}),
			);
			await sleep(2_100);
			const abortStale = await fetch(`${endpoint}/admin/api/multipart-uploads`, {
				method: "DELETE",
				headers: {
					authorization,
					"content-type": "application/x-www-form-urlencoded",
				},
				body: new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
					older_than_seconds: "1",
					confirm: "true",
				}),
			});
			expect(abortStale.status).toBe(204);
			const afterAbort = await fetch(
				`${endpoint}/admin/api/multipart-uploads?${new URLSearchParams({
					namespace: accessKeyId,
					bucket: managementBucket,
				})}`,
				{ headers: { authorization } },
			);
			expect(
				(await afterAbort.json()) as Array<{ upload_id: string }>,
			).toHaveLength(0);
		} finally {
			await s3.send(new DeleteBucketCommand({ Bucket: managementBucket }));
		}
	});

	test("views and configures per-principal dashboard quotas", async () => {
		if (target !== "proxy") return;
		const authorization = `Basic ${btoa(`${managementUsername}:${managementPassword}`)}`;
		const usage = await fetch(
			`${endpoint}/admin/api/quotas?${new URLSearchParams({
				namespace: accessKeyId,
			})}`,
			{ headers: { authorization } },
		);
		expect(usage.status).toBe(200);
		expect((await usage.json()).storage_bytes).toBeGreaterThanOrEqual(0);
		const updated = await fetch(`${endpoint}/admin/api/quotas`, {
			method: "POST",
			headers: {
				authorization,
				"content-type": "application/x-www-form-urlencoded",
			},
			body: new URLSearchParams({
				namespace: accessKeyId,
				max_storage_bytes: "1073741824",
				max_requests_per_minute: "100000",
			}),
		});
		expect(updated.status).toBe(204);
		const effective = await fetch(
			`${endpoint}/admin/api/quotas?${new URLSearchParams({
				namespace: accessKeyId,
			})}`,
			{ headers: { authorization } },
		);
		expect(effective.status).toBe(200);
		expect((await effective.json()).max_storage_bytes).toBe(1_073_741_824);
	});

	test("supports bucket and object CRUD through the AWS S3 API", async () => {
		await s3.send(new CreateBucketCommand({ Bucket: bucket }));
		const buckets = await s3.send(new ListBucketsCommand({}));
		expect(buckets.Buckets?.some((item) => item.Name === bucket)).toBe(true);
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: objectKey,
				Body: objectBody,
				ContentType: "text/plain",
				Metadata: { suite: "regression" },
			}),
		);
		const head = await s3.send(
			new HeadObjectCommand({ Bucket: bucket, Key: objectKey }),
		);
		expect(head.ContentType).toBe("text/plain");
		expect(head.Metadata?.suite).toBe("regression");
		expect(head.ContentLength).toBe(objectBody.length);
		const listing = await s3.send(
			new ListObjectsV2Command({ Bucket: bucket, Prefix: "nested/" }),
		);
		expect(listing.Contents?.map((item) => item.Key)).toEqual([objectKey]);
		const object = await s3.send(
			new GetObjectCommand({ Bucket: bucket, Key: objectKey }),
		);
		expect(await object.Body?.transformToString()).toBe(objectBody);
		const presignedResponse = await fetch(await presignedGetUrl(objectKey));
		expect(presignedResponse.status).toBe(200);
		expect(await presignedResponse.text()).toBe(objectBody);
		const range = await s3.send(
			new GetObjectCommand({ Bucket: bucket, Key: objectKey, Range: "bytes=0-4" }),
		);
		expect(range.$metadata.httpStatusCode).toBe(206);
		expect(range.ContentRange).toBe(`bytes 0-4/${objectBody.length}`);
		expect(await range.Body?.transformToString()).toBe(objectBody.slice(0, 5));
		await expectS3Error(
			s3.send(
				new GetObjectCommand({
					Bucket: bucket,
					Key: objectKey,
					Range: "bytes=999999-",
				}),
			),
			416,
			["InvalidRange", "InvalidRequest"],
		);
		const copiedKey = "nested/copied.txt";
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: copiedKey,
				Body: "old destination",
				Metadata: { stale: "metadata" },
				...(target === "proxy" ? { ACL: "public-read" as const } : {}),
			}),
		);
		await s3.send(
			new CopyObjectCommand({
				Bucket: bucket,
				Key: copiedKey,
				CopySource: `${bucket}/${objectKey}`,
			}),
		);
		const copied = await s3.send(
			new GetObjectCommand({ Bucket: bucket, Key: copiedKey }),
		);
		expect(await copied.Body?.transformToString()).toBe(objectBody);
		const copiedHead = await s3.send(
			new HeadObjectCommand({ Bucket: bucket, Key: copiedKey }),
		);
		expect(copiedHead.Metadata?.stale).toBeUndefined();
		expect(copiedHead.Metadata?.suite).toBe("regression");
		if (target === "proxy") {
			const copiedAnonymous = await fetch(`${endpoint}/${bucket}/${copiedKey}`);
			expect(copiedAnonymous.status).toBe(403);
		}
	});

	test("creates access keys through the IAM API", async () => {
		if (target !== "proxy") return;
		const userName = `regression-user-${Date.now()}`;
		const response = await signedCreateAccessKey(userName);
		expect(response.status).toBe(200);
		const xml = await response.text();
		const createdAccessKey = xml.match(/<AccessKeyId>([^<]+)<\/AccessKeyId>/)?.[1];
		const createdSecretKey = xml.match(/<SecretAccessKey>([^<]+)<\/SecretAccessKey>/)?.[1];
		expect(createdAccessKey).toMatch(/^AKIA[0-9a-f]{16}$/);
		expect(createdSecretKey).toMatch(/^[A-Za-z0-9+/]{40}$/);

		const createdClient = new S3Client({
			endpoint,
			region,
			forcePathStyle: true,
			credentials: {
				accessKeyId: createdAccessKey ?? "",
				secretAccessKey: createdSecretKey ?? "",
			},
		});
		const secondResponse = await signedCreateAccessKey(userName);
		expect(secondResponse.status).toBe(200);
		const secondXml = await secondResponse.text();
		const secondAccessKey = secondXml.match(/<AccessKeyId>([^<]+)<\/AccessKeyId>/)?.[1];
		const secondSecretKey = secondXml.match(/<SecretAccessKey>([^<]+)<\/SecretAccessKey>/)?.[1];
		const secondClient = new S3Client({
			endpoint,
			region,
			forcePathStyle: true,
			credentials: {
				accessKeyId: secondAccessKey ?? "",
				secretAccessKey: secondSecretKey ?? "",
			},
		});
		const sharedBucket = `shared-principal-${Date.now().toString(36)}`;
		try {
			const buckets = await createdClient.send(new ListBucketsCommand({}));
			expect(buckets.$metadata.httpStatusCode).toBe(200);
			await createdClient.send(new CreateBucketCommand({ Bucket: sharedBucket }));
			const sharedBuckets = await secondClient.send(new ListBucketsCommand({}));
			expect(sharedBuckets.Buckets?.some((item) => item.Name === sharedBucket)).toBe(true);

			const listResponse = await signedListAccessKeys(userName);
			expect(listResponse.status).toBe(200);
			const listedKeys = await listResponse.text();
			expect(listedKeys).toContain(createdAccessKey ?? "");
			expect(listedKeys).toContain(secondAccessKey ?? "");
			expect(listedKeys).not.toContain("SecretAccessKey");
			const lastUsedResponse = await signedGetAccessKeyLastUsed(createdAccessKey ?? "");
			expect(lastUsedResponse.status).toBe(200);
			expect(await lastUsedResponse.text()).toContain("LastUsedDate");

			const deactivateResponse = await signedUpdateAccessKey(
				secondAccessKey ?? "",
				"Inactive",
			);
			expect(deactivateResponse.status).toBe(200);
			await expectS3Error(
				secondClient.send(new ListBucketsCommand({})),
				403,
				["AccessDenied"],
			);
			const activateResponse = await signedUpdateAccessKey(
				secondAccessKey ?? "",
				"Active",
			);
			expect(activateResponse.status).toBe(200);
			expect(
				(await secondClient.send(new ListBucketsCommand({}))).Buckets?.some(
					(item) => item.Name === sharedBucket,
				),
			).toBe(true);
			await secondClient.send(new DeleteBucketCommand({ Bucket: sharedBucket }));
			const deleteResponse = await signedDeleteAccessKey(
				createdAccessKey ?? "",
				userName,
			);
			expect(deleteResponse.status).toBe(200);
			expect(await deleteResponse.text()).toContain("DeleteAccessKeyResponse");
			await expectS3Error(
				createdClient.send(new ListBucketsCommand({})),
				403,
				["AccessDenied"],
			);
			const deleteSecondResponse = await signedDeleteAccessKey(
				secondAccessKey ?? "",
				userName,
			);
			expect(deleteSecondResponse.status).toBe(200);
		} finally {
			createdClient.destroy();
			secondClient.destroy();
		}
	});

	test("is usable by rclone", async () => {
		const result = await rclone(["lsf", `regression:${bucket}/nested`]);
		expect(result.stdout).toContain("hello.txt");
		const cat = await rclone(["cat", `regression:${bucket}/${objectKey}`]);
		expect(cat.stdout).toBe(objectBody.trim());
	});

	test("supports rclone mkdir, move, and tree", async () => {
		const sourceKey = "rclone-source.txt";
		const movedKey = "rclone-directory/rclone-source.txt";
		const sourcePath = `${process.cwd()}\\target\\rclone-move-${process.pid}.txt`;
		await Bun.write(sourcePath, "rclone move regression\n");

		try {
			await rclone(["mkdir", `regression:${bucket}/rclone-directory`]);
			await rclone([
				"copyto",
				sourcePath,
				`regression:${bucket}/${sourceKey}`,
			]);
			await rclone([
				"move",
				`regression:${bucket}/${sourceKey}`,
				`regression:${bucket}/rclone-directory/`,
			]);

			const tree = await rclone([
				"tree",
				`regression:${bucket}/rclone-directory`,
			]);
			expect(tree.stdout).toContain("rclone-source.txt");
			await rclone(["deletefile", `regression:${bucket}/${movedKey}`]);
			const treeAfterDelete = await rclone([
				"tree",
				`regression:${bucket}/rclone-directory`,
			]);
			expect(treeAfterDelete.stdout).not.toContain("rclone-source.txt");
		} finally {
			await rclone(["deletefile", `regression:${bucket}/${movedKey}`]).catch(
				() => {},
			);
			await rclone(["deletefile", `regression:${bucket}/${sourceKey}`]).catch(
				() => {},
			);
			await rclone(["rmdir", `regression:${bucket}/rclone-directory`]).catch(
				() => {},
			);
			await unlink(sourcePath).catch(() => {});
		}
	});

	test("supports multipart upload lifecycle", async () => {
		const key = "multipart/assembled.txt";
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: key,
				Body: "old multipart destination",
				Metadata: { stale: "metadata" },
				...(target === "proxy" ? { ACL: "public-read" as const } : {}),
			}),
		);
		const initiated = await s3.send(
			new CreateMultipartUploadCommand({ Bucket: bucket, Key: key }),
		);
		expect(initiated.UploadId).toBeDefined();
		const uploadId = initiated.UploadId ?? "";
		const firstPart = "a".repeat(5 * 1024 * 1024);
		await expectS3Error(
			s3.send(
				new UploadPartCommand({
					Bucket: bucket,
					Key: "multipart/different-key.txt",
					UploadId: uploadId,
					PartNumber: 1,
					Body: "wrong upload",
				}),
			),
			404,
			["NoSuchUpload"],
		);
		const first = await s3.send(
			new UploadPartCommand({
				Bucket: bucket,
				Key: key,
				UploadId: uploadId,
				PartNumber: 1,
				Body: firstPart,
			}),
		);
		const second = await s3.send(
			new UploadPartCommand({
				Bucket: bucket,
				Key: key,
				UploadId: uploadId,
				PartNumber: 2,
				Body: objectBody,
			}),
		);
		expect(first.ETag).toBeDefined();
		expect(second.ETag).toBeDefined();
		const listedParts = await s3.send(
			new ListPartsCommand({ Bucket: bucket, Key: key, UploadId: uploadId }),
		);
		expect(listedParts.Parts?.map((part) => part.PartNumber)).toEqual([1, 2]);
		await expectS3Error(
			s3.send(
				new CompleteMultipartUploadCommand({
					Bucket: bucket,
					Key: key,
					UploadId: uploadId,
					MultipartUpload: {
						Parts: [
							{ ETag: '"wrong"', PartNumber: 1 },
							{ ETag: second.ETag, PartNumber: 2 },
						],
					},
				}),
			),
			400,
			["InvalidPart"],
		);
		await s3.send(
			new CompleteMultipartUploadCommand({
				Bucket: bucket,
				Key: key,
				UploadId: uploadId,
				MultipartUpload: {
					Parts: [
						{ ETag: first.ETag, PartNumber: 1 },
						{ ETag: second.ETag, PartNumber: 2 },
					],
				},
			}),
		);
		const assembled = await s3.send(
			new GetObjectCommand({ Bucket: bucket, Key: key }),
		);
		expect(await assembled.Body?.transformToString()).toBe(
			firstPart + objectBody,
		);
		const multipartHead = await s3.send(
			new HeadObjectCommand({ Bucket: bucket, Key: key }),
		);
		expect(multipartHead.Metadata?.stale).toBeUndefined();
		if (target === "proxy") {
			const multipartAnonymous = await fetch(`${endpoint}/${bucket}/${key}`);
			expect(multipartAnonymous.status).toBe(403);
		}
		const aborted = await s3.send(
			new CreateMultipartUploadCommand({ Bucket: bucket, Key: "multipart/aborted" }),
		);
		await s3.send(
			new AbortMultipartUploadCommand({
				Bucket: bucket,
				Key: "multipart/aborted",
				UploadId: aborted.UploadId,
			}),
		);
		await expectS3Error(
			s3.send(
				new UploadPartCommand({
					Bucket: bucket,
					Key: "multipart/aborted",
					UploadId: aborted.UploadId,
					PartNumber: 1,
					Body: "rejected",
				}),
			),
			404,
			["NoSuchUpload"],
		);
		await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: key }));
	});

	test("enforces public and private object access", async () => {
		if (target !== "proxy") return;
		const publicObjectKey = "public/object.txt";
		const privateObjectKey = "private/object.txt";
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: publicObjectKey,
				Body: objectBody,
				ACL: "public-read",
			}),
		);
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: privateObjectKey,
				Body: objectBody,
			}),
		);
		const publicResponse = await fetch(`${endpoint}/${bucket}/${publicObjectKey}`);
		expect(publicResponse.status).toBe(200);
		expect(await publicResponse.text()).toBe(objectBody);
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: publicObjectKey,
				Body: "private replacement",
			}),
		);
		const overwrittenPublicResponse = await fetch(
			`${endpoint}/${bucket}/${publicObjectKey}`,
		);
		expect(overwrittenPublicResponse.status).toBe(403);

		const bulkPublicKey = "public/bulk-delete.txt";
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: bulkPublicKey,
				Body: objectBody,
				ACL: "public-read",
			}),
		);
		await s3.send(
			new DeleteObjectsCommand({
				Bucket: bucket,
				Delete: { Objects: [{ Key: bulkPublicKey }] },
			}),
		);
		await s3.send(
			new PutObjectCommand({ Bucket: bucket, Key: bulkPublicKey, Body: objectBody }),
		);
		const recreatedBulkResponse = await fetch(
			`${endpoint}/${bucket}/${bulkPublicKey}`,
		);
		expect(recreatedBulkResponse.status).toBe(403);
		await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: bulkPublicKey }));
		const privateResponse = await fetch(
			`${endpoint}/${bucket}/${privateObjectKey}`,
		);
		expect(privateResponse.status).toBe(403);
		const quietDelete = await s3.send(
			new DeleteObjectsCommand({
				Bucket: bucket,
				Delete: {
					Objects: [{ Key: publicObjectKey }, { Key: privateObjectKey }],
					Quiet: true,
				},
			}),
		);
		expect(quietDelete.Deleted ?? []).toHaveLength(0);
		const publicBucket = `${bucket}-public`;
		await s3.send(
			new CreateBucketCommand({ Bucket: publicBucket, ACL: "public-read" }),
		);
		await s3.send(
			new PutObjectCommand({
				Bucket: publicBucket,
				Key: "bucket-public.txt",
				Body: objectBody,
			}),
		);
		const publicBucketResponse = await fetch(
			`${endpoint}/${publicBucket}/bucket-public.txt`,
		);
		expect(publicBucketResponse.status).toBe(200);
		await s3.send(
			new DeleteObjectCommand({
				Bucket: publicBucket,
				Key: "bucket-public.txt",
			}),
		);
		await s3.send(new DeleteBucketCommand({ Bucket: publicBucket }));
	});

	test("stores and evaluates bucket policies", async () => {
		if (target !== "proxy") return;
		const policyObjectKey = "policy/public.txt";
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: policyObjectKey,
				Body: objectBody,
			}),
		);
		const policy = JSON.stringify({
			Version: "2012-10-17",
			Statement: [
				{
					Effect: "Allow",
					Principal: "*",
					Action: ["s3:GetObject", "s3:PutObject", "s3:DeleteObject"],
					Resource: `arn:aws:s3:::${bucket}/policy/*`,
				},
				{
					Effect: "Allow",
					Principal: "*",
					Action: "s3:ListBucket",
					Resource: `arn:aws:s3:::${bucket}`,
				},
			],
		});
		await s3.send(new PutBucketPolicyCommand({ Bucket: bucket, Policy: policy }));
		const stored = await s3.send(
			new GetBucketPolicyCommand({ Bucket: bucket }),
		);
		expect(JSON.parse(stored.Policy ?? "{}")).toEqual(JSON.parse(policy));

		const allowed = await fetch(`${endpoint}/${bucket}/${policyObjectKey}`);
		expect(allowed.status).toBe(200);
		expect(await allowed.text()).toBe(objectBody);
		const denied = await fetch(`${endpoint}/${bucket}/${objectKey}`);
		expect(denied.status).toBe(403);
		const anonymousPutKey = "policy/anonymous-put.txt";
		const anonymousPut = await fetch(
			`${endpoint}/${bucket}/${anonymousPutKey}`,
			{ method: "PUT", body: objectBody },
		);
		expect(anonymousPut.status).toBe(200);
		const anonymousList = await fetch(`${endpoint}/${bucket}?list-type=2`);
		expect(anonymousList.status).toBe(200);
		expect(await anonymousList.text()).toContain(anonymousPutKey);
		const anonymousDelete = await fetch(
			`${endpoint}/${bucket}/${anonymousPutKey}`,
			{ method: "DELETE" },
		);
		expect(anonymousDelete.status).toBe(204);
		const signedPolicyKey = "policy/signed.txt";
		await s3.send(
			new PutObjectCommand({ Bucket: bucket, Key: signedPolicyKey, Body: objectBody }),
		);
		const signedPolicyList = await s3.send(
			new ListObjectsV2Command({ Bucket: bucket, Prefix: "policy/" }),
		);
		expect(signedPolicyList.Contents?.some((item) => item.Key === signedPolicyKey)).toBe(
			true,
		);
		await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: signedPolicyKey }));

		await s3.send(new DeleteBucketPolicyCommand({ Bucket: bucket }));
		const revoked = await fetch(`${endpoint}/${bucket}/${policyObjectKey}`);
		expect(revoked.status).toBe(403);
		await s3.send(
			new DeleteObjectCommand({ Bucket: bucket, Key: policyObjectKey }),
		);
		const deletedPolicyBucket = `${bucket}-policy-delete`;
		const deletedPolicyKey = "object.txt";
		await s3.send(new CreateBucketCommand({ Bucket: deletedPolicyBucket }));
		await s3.send(
			new PutBucketPolicyCommand({
				Bucket: deletedPolicyBucket,
				Policy: JSON.stringify({
					Version: "2012-10-17",
					Statement: [{
						Effect: "Allow",
						Principal: "*",
						Action: "s3:GetObject",
						Resource: `arn:aws:s3:::${deletedPolicyBucket}/*`,
					}],
				}),
			}),
		);
		await s3.send(
			new PutObjectCommand({
				Bucket: deletedPolicyBucket,
				Key: deletedPolicyKey,
				Body: objectBody,
			}),
		);
		await s3.send(new DeleteBucketCommand({ Bucket: deletedPolicyBucket }));
		await s3.send(new CreateBucketCommand({ Bucket: deletedPolicyBucket }));
		await s3.send(
			new PutObjectCommand({
				Bucket: deletedPolicyBucket,
				Key: deletedPolicyKey,
				Body: objectBody,
			}),
		);
		const recreatedPolicyResponse = await fetch(
			`${endpoint}/${deletedPolicyBucket}/${deletedPolicyKey}`,
		);
		expect(recreatedPolicyResponse.status).toBe(403);
		await s3.send(new DeleteObjectCommand({ Bucket: deletedPolicyBucket, Key: deletedPolicyKey }));
		await s3.send(new DeleteBucketCommand({ Bucket: deletedPolicyBucket }));
	});

	test("preserves object versions and delete markers", async () => {
		const versionedBucket = `versioned-${Date.now().toString(36)}`;
		const key = "versioned.txt";
		await s3.send(new CreateBucketCommand({ Bucket: versionedBucket }));
		try {
			await s3.send(
				new PutBucketVersioningCommand({
					Bucket: versionedBucket,
					VersioningConfiguration: { Status: "Enabled" },
				}),
			);
			const versioning = await s3.send(
				new GetBucketVersioningCommand({ Bucket: versionedBucket }),
			);
			expect(versioning.Status).toBe("Enabled");
			const first = await s3.send(
				new PutObjectCommand({ Bucket: versionedBucket, Key: key, Body: "first" }),
			);
			const second = await s3.send(
				new PutObjectCommand({ Bucket: versionedBucket, Key: key, Body: "second" }),
			);
			expect(first.VersionId).toBeTruthy();
			expect(second.VersionId).toBeTruthy();
			expect(first.VersionId).not.toBe(second.VersionId);
			const historical = await s3.send(
				new GetObjectCommand({
					Bucket: versionedBucket,
					Key: key,
					VersionId: first.VersionId,
				}),
			);
			expect(await historical.Body?.transformToString()).toBe("first");
			const deleted = await s3.send(
				new DeleteObjectCommand({ Bucket: versionedBucket, Key: key }),
			);
			expect(deleted.DeleteMarker).toBe(true);
			expect(deleted.VersionId).toBeTruthy();
			await expectS3Error(
				s3.send(new GetObjectCommand({ Bucket: versionedBucket, Key: key })),
				404,
				["NoSuchKey"],
			);
			const versions = await s3.send(
				new ListObjectVersionsCommand({ Bucket: versionedBucket }),
			);
			expect(versions.Versions?.map((version) => version.VersionId)).toContain(
				first.VersionId,
			);
			expect(versions.DeleteMarkers?.map((marker) => marker.VersionId)).toContain(
				deleted.VersionId,
			);
		} finally {
			await s3
				.send(new DeleteBucketCommand({ Bucket: versionedBucket }))
				.catch(() => {});
		}
	});

	test("lists 2000 objects through paginated responses", async () => {
		const started = performance.now();
		const objects = Array.from({ length: 2000 }, (_, index) => ({
			Key: `bulk/${index.toString().padStart(4, "0")}.txt`,
			Body: `bulk object ${index}`,
		}));
		for (let offset = 0; offset < objects.length; offset += batchSize) {
			await Promise.all(
				objects
					.slice(offset, offset + batchSize)
					.map((object) =>
						s3.send(new PutObjectCommand({ Bucket: bucket, ...object })),
					),
			);
		}
		if (profileRegression)
			console.log(
				`[profile] upload: ${(performance.now() - started).toFixed(0)}ms`,
			);

		const keys: string[] = [];
		const listingStarted = performance.now();
		let continuationToken: string | undefined;
		let pageCount = 0;
		do {
			const page = await s3.send(
				new ListObjectsV2Command({
					Bucket: bucket,
					Prefix: "bulk/",
					MaxKeys: 1000,
					ContinuationToken: continuationToken,
				}),
			);
			pageCount += 1;
			keys.push(
				...(page.Contents?.flatMap((object) =>
					object.Key ? [object.Key] : [],
				) ?? []),
			);
			continuationToken = page.NextContinuationToken;
		} while (continuationToken);
		if (profileRegression)
			console.log(
				`[profile] list: ${(performance.now() - listingStarted).toFixed(0)}ms`,
			);

		expect(pageCount).toBe(2);
		expect(keys).toHaveLength(2000);
		expect(new Set(keys).size).toBe(2000);
		expect(keys).toContain("bulk/0000.txt");
		expect(keys).toContain("bulk/1999.txt");

		const deletionStarted = performance.now();
		for (let offset = 0; offset < objects.length; offset += batchSize) {
			await s3.send(
				new DeleteObjectsCommand({
					Bucket: bucket,
					Delete: {
						Objects: objects
							.slice(offset, offset + batchSize)
							.map(({ Key }) => ({ Key })),
						Quiet: true,
					},
				}),
			);
		}
		if (profileRegression)
			console.log(
				`[profile] delete: ${(performance.now() - deletionStarted).toFixed(0)}ms, total: ${(performance.now() - started).toFixed(0)}ms`,
			);
	}, 90_000);

	test("returns S3 errors for missing resources", async () => {
		await expectS3Error(
			s3.send(new GetObjectCommand({ Bucket: bucket, Key: "missing.txt" })),
			404,
			["NoSuchKey", "NotFound"],
		);
		await expectS3Error(
			s3.send(new ListObjectsV2Command({ Bucket: `${bucket}-missing` })),
			404,
			["NoSuchBucket", "NotFound"],
		);
	});

	test("rejects malformed multi-object delete XML", async () => {
		if (target !== "proxy") return;
		const response = await signedDelete("<Delete><Object>");
		expect(response.status).toBe(400);
		expect(await response.text()).toContain("MalformedXML");
	});

	test("rejects an invalid multi-object delete Content-MD5", async () => {
		if (target !== "proxy") return;
		const response = await signedDelete(
			"<Delete><Object><Key>missing.txt</Key></Object></Delete>",
			"AAAAAAAAAAAAAAAAAAAAAA==",
		);
		expect(response.status).toBe(400);
		expect(await response.text()).toContain("BadDigest");
	});

	test("returns per-object delete failures", async () => {
		if (target !== "proxy" || opendalProvider !== "fs") return;
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: "delete-failure/child.txt",
				Body: objectBody,
			}),
		);
		await s3.send(
			new PutObjectCommand({
				Bucket: bucket,
				Key: "delete-success.txt",
				Body: objectBody,
			}),
		);
		const result = await s3.send(
			new DeleteObjectsCommand({
				Bucket: bucket,
				Delete: {
					Objects: [
						{ Key: "delete-failure" },
						{ Key: "delete-success.txt" },
					],
				},
			}),
		);
		expect(result.Deleted?.map(({ Key }) => Key)).toEqual([
			"delete-success.txt",
		]);
		expect(result.Errors?.map(({ Key }) => Key)).toContain("delete-failure");
		await s3.send(
			new DeleteObjectCommand({
				Bucket: bucket,
				Key: "delete-failure/child.txt",
			}),
		);
	});

	test("rejects more than 1000 objects in a multi-object delete", async () => {
		if (target !== "proxy") return;
		await expectS3Error(
			s3.send(
				new DeleteObjectsCommand({
					Bucket: bucket,
					Delete: {
						Objects: Array.from({ length: 1001 }, (_, index) => ({
							Key: `too-many/${index}`,
						})),
					},
				}),
			),
			400,
			["MalformedXML", "InvalidRequest"],
		);
	});

	test("rejects invalid credentials", async () => {
		const invalid = new S3Client({
			endpoint,
			region,
			forcePathStyle: true,
			credentials: {
				accessKeyId: "invalid-access-key",
				secretAccessKey: "invalid-secret-key",
			},
		});
		try {
			await expectS3Error(invalid.send(new ListBucketsCommand({})), 403, [
				"AccessDenied",
				"InvalidAccessKeyId",
			]);
		} finally {
			invalid.destroy();
		}
	});

	test("accepts a browser-style presigned POST", async () => {
		const post = await createPresignedPost(s3, {
			Bucket: bucket,
			Key: "uploads/$" + "{filename}",
			Conditions: [["content-length-range", 1, 1024]],
			Fields: { "Content-Type": "text/plain" },
			Expires: 300,
		});
		const form = new FormData();
		for (const [key, value] of Object.entries(post.fields))
			form.append(key, value);
		form.append(
			"file",
			new Blob([objectBody], { type: "text/plain" }),
			"posted.txt",
		);
		const response = await fetch(post.url, { method: "POST", body: form });
		expect(response.ok).toBe(true);
		const posted = await s3.send(
			new GetObjectCommand({ Bucket: bucket, Key: "uploads/posted.txt" }),
		);
		expect(await posted.Body?.transformToString()).toBe(objectBody);

		const invalidForm = new FormData();
		for (const [key, value] of Object.entries(post.fields)) {
			invalidForm.append(
				key,
				key.toLowerCase() === "x-amz-signature" ? "invalid" : value,
			);
		}
		invalidForm.append(
			"file",
			new Blob([objectBody], { type: "text/plain" }),
			"rejected.txt",
		);
		const rejected = await fetch(post.url, {
			method: "POST",
			body: invalidForm,
		});
		expect([400, 403]).toContain(rejected.status);
	});

	test("deletes objects and buckets", async () => {
		await Promise.all(
			["bulk-delete-a.txt", "bulk-delete-b.txt"].map((Key) =>
				s3.send(new PutObjectCommand({ Bucket: bucket, Key, Body: objectBody })),
			),
		);
		const deleteResult = await s3.send(
			new DeleteObjectsCommand({
				Bucket: bucket,
				Delete: {
					Objects: [
						{ Key: "bulk-delete-a.txt" },
						{ Key: "bulk-delete-b.txt" },
						{ Key: "already-missing.txt" },
					],
				},
			}),
		);
		expect(deleteResult.Deleted?.map(({ Key }) => Key)).toEqual([
			"bulk-delete-a.txt",
			"bulk-delete-b.txt",
			"already-missing.txt",
		]);
		await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: objectKey }));
		await s3.send(
			new DeleteObjectCommand({ Bucket: bucket, Key: "nested/copied.txt" }),
		);
		await s3.send(
			new DeleteObjectCommand({ Bucket: bucket, Key: "uploads/posted.txt" }),
		);
		await s3.send(
			new DeleteObjectCommand({ Bucket: bucket, Key: "uploads/rejected.txt" }),
		);
		await s3.send(new DeleteBucketCommand({ Bucket: bucket }));
		await expect(
			s3.send(new HeadObjectCommand({ Bucket: bucket, Key: objectKey })),
		).rejects.toBeDefined();
	});
});
