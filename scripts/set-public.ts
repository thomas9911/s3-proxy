import { PutBucketPolicyCommand, S3Client } from "@aws-sdk/client-s3";

function parseArguments() {
  return {
    bucket: "storage",
    "external-url": "http://localhost:3000",
    "access-key": "admin",
    "secret-key": "admin-secret-key-change-me",
  };
}

const settings = parseArguments();
for (let attempt = 0; attempt < 60; attempt += 1) {
  try {
    const response = await fetch(`${settings["external-url"]}/healthz`);
    if (response.ok) break;
  } catch {
    // The proxy is still starting.
  }
  if (attempt === 59)
    throw new Error("Proxy did not become ready within 30 seconds");
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
  await s3.send(
    new PutBucketPolicyCommand({ Bucket: settings.bucket, Policy: policy }),
  );
  console.log(`Configured s3://${settings.bucket}/ for public reads`);
} finally {
  s3.destroy();
}
