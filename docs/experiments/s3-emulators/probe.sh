#!/bin/sh
# S3 operations needed by POC-023, probed with the AWS CLI.
# Usage: see README.md. Runs inside amazon/aws-cli with entrypoint sh.
E="$1"; B=probe-$(date +%s)
a() { aws --endpoint-url "$E" --region us-east-1 s3api "$@" 2>&1; }
r() { printf '%-34s %s\n' "$1" "$2"; }
head -c 6000000 /dev/urandom > /tmp/p1; head -c 1000 /dev/urandom > /tmp/p2; echo hello > /tmp/small
a create-bucket --bucket $B >/dev/null && r create-bucket ok || r create-bucket FAIL
o=$(a put-object --bucket $B --key c.json --body /tmp/small --if-none-match '*'); echo "$o" | grep -q ETag && r "put if-none-match (new)" ok || r "put if-none-match (new)" "FAIL $o"
o=$(a put-object --bucket $B --key c.json --body /tmp/small --if-none-match '*'); echo "$o" | grep -q PreconditionFailed && r "put if-none-match (exists)" "412 ok" || r "put if-none-match (exists)" "NOT REJECTED: $(echo $o | head -c 120)"
o=$(a put-object --bucket $B --key s.bin --body /tmp/small --checksum-algorithm SHA256); echo "$o" | grep -q ChecksumSHA256 && r "put checksum sha256" ok || r "put checksum sha256" "no checksum echoed: $(echo $o | head -c 120)"
o=$(a head-object --bucket $B --key s.bin --checksum-mode ENABLED); echo "$o" | grep -q ChecksumSHA256 && r "head checksum-mode" ok || r "head checksum-mode" "missing"
o=$(a put-object --bucket $B --key bad.bin --body /tmp/small --checksum-sha256 AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=); echo "$o" | grep -qiE 'BadDigest|InvalidRequest|mismatch|XAmzContentChecksumMismatch|InvalidDigest|400' && r "bad checksum rejected" ok || r "bad checksum rejected" "ACCEPTED: $(echo $o | head -c 100)"
U=$(a create-multipart-upload --bucket $B --key big.bin --query UploadId --output text); r "multipart upload id" "$U"
a upload-part --bucket $B --key big.bin --upload-id "$U" --part-number 1 --body /tmp/p1 >/dev/null
o=$(a list-multipart-uploads --bucket $B); echo "$o" | grep -q "$U" && r list-multipart-uploads ok || r list-multipart-uploads "missing: $(echo $o|head -c 100)"
a abort-multipart-upload --bucket $B --key big.bin --upload-id "$U" >/dev/null
o=$(a list-multipart-uploads --bucket $B); echo "$o" | grep -q "$U" && r "abort multipart" "STILL LISTED" || r "abort multipart" ok
U=$(a create-multipart-upload --bucket $B --key big2.bin --query UploadId --output text)
T1=$(a upload-part --bucket $B --key big2.bin --upload-id "$U" --part-number 1 --body /tmp/p1 --query ETag --output text)
T2=$(a upload-part --bucket $B --key big2.bin --upload-id "$U" --part-number 2 --body /tmp/p2 --query ETag --output text)
o=$(a complete-multipart-upload --bucket $B --key big2.bin --upload-id "$U" --multipart-upload "{\"Parts\":[{\"PartNumber\":1,\"ETag\":$T1},{\"PartNumber\":2,\"ETag\":$T2}]}"); echo "$o" | grep -q ETag && r complete-multipart ok || r complete-multipart "FAIL $(echo $o|head -c 120)"
aws --endpoint-url "$E" --region us-east-1 s3 cp s3://$B/big2.bin /tmp/back >/dev/null 2>&1; [ "$(cat /tmp/p1 /tmp/p2 | md5sum | cut -c1-32)" = "$(md5sum < /tmp/back | cut -c1-32)" ] && r "multipart readback bytes" ok || r "multipart readback bytes" FAIL
o=$(a get-object --bucket $B --key big2.bin --range bytes=10-19 /tmp/rng); [ "$(wc -c < /tmp/rng)" = 10 ] && r "ranged get" ok || r "ranged get" FAIL
a delete-object --bucket $B --key big2.bin >/dev/null; o=$(a head-object --bucket $B --key big2.bin); echo "$o" | grep -qE '404|Not Found' && r delete-object ok || r delete-object FAIL

# Paging and batch delete on a fresh bucket; the bucket name is kept for the restart check.
B=pg-$(date +%s%N | cut -c1-13)
a() { aws --endpoint-url "$E" --region us-east-1 s3api "$@" 2>&1; }
a create-bucket --bucket $B >/dev/null; echo x > /tmp/x
for k in 1 2 3; do a put-object --bucket $B --key sets/$k --body /tmp/x >/dev/null; done
o=$(a list-objects-v2 --bucket $B --prefix sets/ --max-keys 1); T=$(echo "$o" | grep -o '"NextContinuationToken": "[^"]*"' | cut -d'"' -f4)
o2=$(a list-objects-v2 --bucket $B --prefix sets/ --max-keys 5 --continuation-token "$T"); n=$(echo "$o2" | grep -c '"Key"')
[ -n "$T" ] && [ "$n" = 2 ] && echo "list v2 paging                     ok" || echo "list v2 paging                     FAIL ($n)"
o=$(a delete-objects --bucket $B --delete '{"Objects":[{"Key":"sets/1"},{"Key":"sets/2"}]}'); n=$(a list-objects-v2 --bucket $B --prefix sets/ | grep -c '"Key"'); [ "$n" = 1 ] && echo "delete-objects (batch)             ok" || echo "delete-objects (batch)             FAIL $o"
echo $B > /p/bucket
