import { afterAll, beforeAll, expect, test } from "bun:test";
import { createServer } from "node:net";
import {
  S3Client,
  CreateBucketCommand,
  PutObjectCommand,
  GetObjectCommand,
  DeleteObjectCommand,
  ListObjectsV2Command,
} from "@aws-sdk/client-s3";

let proxy: Bun.Subprocess;
let client: S3Client;
let endpoint: string;

beforeAll(async () => {
  const socket = createServer();
  await new Promise<void>((resolve) => socket.listen(0, "127.0.0.1", resolve));
  const address = socket.address();
  if (!address || typeof address === "string")
    throw new Error("No test port available");
  const port = address.port;
  await new Promise<void>((resolve, reject) =>
    socket.close((error) => (error ? reject(error) : resolve())),
  );
  endpoint = `http://127.0.0.1:${port}`;
  const env = Object.fromEntries(
    Object.entries(process.env).filter(
      ([key]) => !key.startsWith("S3_PROXY__"),
    ),
  );
  proxy = Bun.spawn(
    [
      "cargo",
      "run",
      "--quiet",
      "--no-default-features",
      "--features",
      "management",
    ],
    {
      env: {
        ...env,
        S3_PROXY__SERVER_HOST: `127.0.0.1:${port}`,
        S3_PROXY__METADATA_BACKEND: "sqlite",
        S3_PROXY__SQLITE__URL: "sqlite::memory:",
        S3_PROXY__OPENDAL_PROVIDER: "memory",
        S3_PROXY__ADMIN__ACCESS_KEY: "admin",
        S3_PROXY__ADMIN__SECRET_KEY: "test-secret-key",
        S3_PROXY__MANAGEMENT__USERNAME: "dashboard",
        S3_PROXY__MANAGEMENT__PASSWORD: "dashboard-secret",
      },
      stdout: "ignore",
      stderr: "inherit",
    },
  );
  client = new S3Client({
    endpoint,
    region: "us-east-1",
    forcePathStyle: true,
    credentials: { accessKeyId: "admin", secretAccessKey: "test-secret-key" },
  });
  for (let attempt = 0; attempt < 300; attempt++) {
    if (proxy.exitCode !== null)
      throw new Error(`Proxy exited with code ${proxy.exitCode}`);
    try {
      if ((await fetch(`${endpoint}/readyz`)).ok) return;
    } catch {
      /* Starting. */
    }
    await Bun.sleep(200);
  }
  throw new Error("Proxy did not become ready");
}, 120_000);

afterAll(async () => {
  client?.destroy();
  if (proxy && proxy.exitCode === null) {
    proxy.kill();
    await proxy.exited;
  }
});

test("serves objects through the S3 API", async () => {
  const Bucket = "storage",
    Key = "photos/cat.jpg";
  await client.send(new CreateBucketCommand({ Bucket }));
  await client.send(new PutObjectCommand({ Bucket, Key, Body: "cat" }));
  const list = await client.send(new ListObjectsV2Command({ Bucket }));
  expect(list.Contents?.map((item) => item.Key)).toContain(Key);
  const object = await client.send(new GetObjectCommand({ Bucket, Key }));
  expect(await object.Body?.transformToString()).toBe("cat");
  await client.send(new DeleteObjectCommand({ Bucket, Key }));
  await expect(
    client.send(new GetObjectCommand({ Bucket, Key })),
  ).rejects.toMatchObject({ $metadata: { httpStatusCode: 404 } });
});

test("management dashboard supports browser bucket creation and file upload", async () => {
  const authorization = `Basic ${btoa("dashboard:dashboard-secret")}`;
  expect((await fetch(`${endpoint}/admin`)).status).toBe(401);
  await using view = new Bun.WebView({
    backend: { type: "chrome", url: false },
    width: 1280,
    height: 900,
  });
  await view.navigate("about:blank");
  await view.cdp("Network.enable");
  await view.cdp("Network.setExtraHTTPHeaders", {
    headers: { Authorization: authorization },
  });
  await view.navigate(`${endpoint}/admin`);
  expect(await view.evaluate<string>("document.title")).toBe(
    "S3 Proxy Management",
  );
  const bucketForm = 'form[hx-post="/admin/fragments/buckets"]';
  await view.scrollTo(`${bucketForm} input[name="name"]`);
  await view.click(`${bucketForm} input[name="name"]`);
  await view.type("browser-bucket");
  await view.click(`${bucketForm} button`);
  for (
    let i = 0;
    i < 100 &&
    !(await view.evaluate(
      'document.querySelector("#bucket-list")?.textContent?.includes("browser-bucket")',
    ));
    i++
  )
    await Bun.sleep(100);
  expect(
    await view.evaluate<boolean>(
      'document.querySelector("#bucket-list")?.textContent?.includes("browser-bucket")',
    ),
  ).toBe(true);
  await view.reload();
  const uploadForm = 'form[action="/admin/api/objects"]';
  await view.evaluate(
    `document.querySelector('${uploadForm} select[name="bucket"]').value = 'browser-bucket'`,
  );
  const { root } = (await view.cdp("DOM.getDocument")) as {
    root: { nodeId: number };
  };
  const { nodeId } = (await view.cdp("DOM.querySelector", {
    nodeId: root.nodeId,
    selector: `${uploadForm} input[type="file"]`,
  })) as { nodeId: number };
  await view.cdp("DOM.setFileInputFiles", {
    nodeId,
    files: [Bun.fileURLToPath(new URL("../package.json", import.meta.url))],
  });
  await view.scrollTo(`${uploadForm} button`);
  await view.click(`${uploadForm} button`);
  for (let i = 0; i < 100; i++) {
    try {
      const object = await client.send(
        new GetObjectCommand({ Bucket: "browser-bucket", Key: "package.json" }),
      );
      expect(await object.Body?.transformToString()).toBe(
        await Bun.file("package.json").text(),
      );
      return;
    } catch {
      await Bun.sleep(100);
    }
  }
  throw new Error("Dashboard upload did not appear through S3");
}, 30_000);
