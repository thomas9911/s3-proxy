import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import {
	CreateBucketCommand,
	DeleteBucketCommand,
	DeleteObjectCommand,
	GetObjectCommand,
	HeadObjectCommand,
	ListBucketsCommand,
	ListObjectsV2Command,
	PutObjectCommand,
	S3Client,
} from "@aws-sdk/client-s3";
import { createPresignedPost } from "@aws-sdk/s3-presigned-post";

const target =
	process.env.S3_TEST_TARGET ??
	(process.env.S3_TEST_ENDPOINT ? "external" : "minio");
const endpoint = process.env.S3_TEST_ENDPOINT ?? "http://127.0.0.1:19000";
const accessKeyId = process.env.S3_TEST_ACCESS_KEY ?? "minioadmin";
const secretAccessKey = process.env.S3_TEST_SECRET_KEY ?? "minioadmin";
const region = process.env.S3_TEST_REGION ?? "us-east-1";
const bucket = `regression-${Date.now().toString(36)}`;
const objectKey = "nested/hello.txt";
const objectBody = "s3-proxy regression test\n";
const containerName = `s3-proxy-regression-${process.pid}`;
const redisContainerName = `s3-proxy-regression-redis-${process.pid}`;
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
		proxyProcess = Bun.spawn(["cargo", "run", "--quiet"], {
			env: {
				...process.env,
				S3_PROXY__SERVER_HOST: "0.0.0.0:19000",
				S3_PROXY__EXTERNAL_SERVER_HOST: endpoint,
				S3_PROXY__REDIS__URL: "redis://127.0.0.1:16379",
				S3_PROXY__OPENDAL_PROVIDER: "memory",
				S3_PROXY__OPENDAL__ROOT: "/tmp",
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
});
afterAll(async () => {
	s3?.destroy();
	proxyProcess?.kill();
	if (ownsContainer)
		run(["docker", "rm", "--force", containerName], { check: false });
	if (target === "proxy")
		run(["docker", "rm", "--force", redisContainerName], { check: false });
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
	});

	test("is usable by rclone", async () => {
		const result = await rclone(["lsf", `regression:${bucket}/nested`]);
		expect(result.stdout).toContain("hello.txt");
		const cat = await rclone(["cat", `regression:${bucket}/${objectKey}`]);
		expect(cat.stdout).toBe(objectBody.trim());
	});

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
		await s3.send(new DeleteObjectCommand({ Bucket: bucket, Key: objectKey }));
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
