# S3 emulators for replication tests

Research date: 2026-11-02. Question: which local container can stand in for R2 or S3 so POC-023 can be tested end to end without a real bucket, now that MinIO is no longer an option.

## Why not MinIO or LocalStack

MinIO's community edition stopped publishing binaries and Docker images in October 2025, entered maintenance mode in December 2025, and its repository was archived in 2026. It remains AGPLv3 but is source-only; the maintained product (AIStor) needs a license key. [StableBuild](https://www.stablebuild.com/blog/minio-images-disappeared-from-docker-hub), [Pinggy](https://pinggy.io/blog/minio_archived_self_hosted_s3_alternatives/).

LocalStack's open-source repository was archived in March 2026, and current images require an account and auth token. [DEV Community](https://dev.to/peytongreen_dev/localstack-killed-its-free-tier-heres-how-to-test-aws-in-python-for-free-in-2026-12me). Both are excluded.

## Candidates tested

All three are Apache 2.0 and run as one container with access keys from environment variables.

| Image | Version | Size | Result |
| --- | --- | --- | --- |
| `chrislusf/seaweedfs:4.50` (`sha256:c2658820e763294a9e9de7b36c81e59b879e3459148351aee4e31a126ca64860`) | 4.50 | 735 MB | All checks pass, including restart persistence. **Selected.** |
| `rustfs/rustfs:latest` (`sha256:ed9210d6411d77ffceb70d94b964a3cb7d225983368bc25bebe01e995293dcc2`) | 1.0.4 | 420 MB | All checks pass. Younger project; fallback. |
| `versity/versitygw:latest` (`sha256:89cad13cc981986ee284cbafea4613f9533ce6fd5f37a2c96c4c4fb8adb3a316`) | 1.10.0 | 118 MB | All checks pass. The restart check failed only because the probe used tmpfs storage. Stores objects as plain files. |

Not tested: Garage (AGPLv3, needs an init step and TOML config, and its compatibility page lists no conditional writes), Zenko CloudServer, S3Proxy (its jclouds dependency was retired), and Ceph RGW (too heavy for a test container). [rmoff](https://rmoff.net/2026/01/14/alternatives-to-minio-for-single-node-local-s3/), [Garage S3 compatibility](https://garagehq.deuxfleurs.fr/documentation/reference-manual/s3-compatibility/).

SeaweedFS is selected because it is the most established of the three (S3 since 2018), needs only `server -s3`, and passed every check. RustFS is a drop-in fallback if SeaweedFS regresses.

## What the probe checks

[probe.sh](probe.sh) uses the AWS CLI against a fresh bucket and covers the operations the replication worker needs:

- `PutObject` with `If-None-Match: *` creates a new key and returns `412` for an existing one, so the remote completion record is published without replacement.
- `x-amz-checksum-sha256` is stored and returned by `HeadObject --checksum-mode ENABLED`, and a wrong checksum is rejected.
- Multipart create, upload, list, abort (gone from the listing), and complete; the readback matches the uploaded bytes.
- Ranged `GetObject`, `ListObjectsV2` paging with a continuation token, `DeleteObject`, and batch `DeleteObjects` (retention).
- Objects survive a container restart (run separately, below).

Each passes against all three images. An emulator does not prove R2's behavior; R2 implements a subset of S3, so the opt-in live check in POC-023 covers it.

## Reproduce

```sh
docker run -d --name s3p -p 127.0.0.1:18333:8333 \
	-e AWS_ACCESS_KEY_ID=probeaccesskey01 -e AWS_SECRET_ACCESS_KEY=probesecretkey0123456789abcdef \
	chrislusf/seaweedfs:4.50 server -dir=/data -s3 -s3.port=8333
sleep 10
out=$(mktemp -d)
docker run --rm --network host \
	-e AWS_ACCESS_KEY_ID=probeaccesskey01 -e AWS_SECRET_ACCESS_KEY=probesecretkey0123456789abcdef \
	-v "$PWD/docs/experiments/s3-emulators:/s:ro" -v "$out:/p" \
	--entrypoint sh amazon/aws-cli /s/probe.sh http://127.0.0.1:18333
docker restart s3p && sleep 10
docker run --rm --network host \
	-e AWS_ACCESS_KEY_ID=probeaccesskey01 -e AWS_SECRET_ACCESS_KEY=probesecretkey0123456789abcdef \
	amazon/aws-cli --endpoint-url http://127.0.0.1:18333 --region us-east-1 \
	s3api list-objects-v2 --bucket "$(cat "$out/bucket")"   # one key, sets/3
docker rm -f s3p
```

Every line should end in `ok`. The credentials are synthetic and only valid for the throwaway container.
