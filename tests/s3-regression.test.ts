import { Database } from "bun:sqlite";
import { SQL } from "bun";
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
	HeadObjectCommand,
	ListBucketsCommand,
	ListPartsCommand,
	ListObjectsV2Command,
	PutObjectCommand,
	PutBucketPolicyCommand,
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
	});
	const signed = await signer.sign(request);
	return fetch(targetUrl, {
		method: "POST",
		headers: signed.headers,
		body,
	});
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
	if (target === "proxy")
		run(["docker", "rm", "--force", redisContainerName], { check: false });
	if (target === "proxy")
		run(["docker", "rm", "--force", postgresContainerName], { check: false });
	if (target === "proxy" && ownsSqliteDatabase)
		await unlink(sqliteDatabasePath).catch(() => {});
});

describe("S3 compatibility contract", () => {
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

	test("is usable by rclone", async () => {
		const result = await rclone(["lsf", `regression:${bucket}/nested`]);
		expect(result.stdout).toContain("hello.txt");
		const cat = await rclone(["cat", `regression:${bucket}/${objectKey}`]);
		expect(cat.stdout).toBe(objectBody.trim());
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
	}, 30_000);

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
