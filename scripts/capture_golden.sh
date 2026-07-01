#!/usr/bin/env bash
# 公式 Lavalink v4 を Docker で起動し、代表的なレスポンスを golden として保存する。
# これにより、本実装の出力と「実物の公式サーバー」の出力を値レベルで突き合わせできる
# （特にフェーズ5 で実 YouTube 抽出を実装したあとの loadtracks 値の差分検証に使う）。
#
# 必要: docker, curl, jq, python3
# 使い方: bash scripts/capture_golden.sh
set -euo pipefail

PW="youshallnotpass"
PORT="2333"
IMAGE="ghcr.io/lavalink-devs/lavalink:4"   # 公式イメージ
OUT="$(cd "$(dirname "$0")/.." && pwd)/crates/server/tests/golden/official"
SAMPLE="QAAAjQIAJVJpY2sgQXN0bGV5IC0gTmV2ZXIgR29ubmEgR2l2ZSBZb3UgVXAADlJpY2tBc3RsZXlWRVZPAAAAAAADPCAAC2RRdzR3OVdnWGNRAAEAK2h0dHBzOi8vd3d3LnlvdXR1YmUuY29tL3dhdGNoP3Y9ZFF3NHc5V2dYY1EAB3lvdXR1YmUAAAAAAAAAAA=="

mkdir -p "$OUT"

# 注意: Lavalink v4 では YouTube は組み込みではなく youtube-source プラグインが必要。
# loadtracks の実値 golden が欲しい場合は application.yml と plugins/ を用意してから起動すること。
echo ">> starting official lavalink ($IMAGE)"
docker rm -f ll-official >/dev/null 2>&1 || true
docker run --rm -d --name ll-official -p "${PORT}:2333" \
  -e "LAVALINK_SERVER_PASSWORD=${PW}" "$IMAGE" >/dev/null

cleanup() { docker stop ll-official >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo ">> waiting for readiness..."
for _ in $(seq 1 60); do
  curl -fsS "localhost:${PORT}/version" >/dev/null 2>&1 && break
  sleep 1
done

AUTH=(-H "Authorization: ${PW}")
enc() { python3 -c 'import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=""))' "$1"; }

echo ">> capturing goldens to $OUT"
curl -fsS "${AUTH[@]}" "localhost:${PORT}/v4/info"  | jq . > "$OUT/info.json"
curl -fsS "${AUTH[@]}" "localhost:${PORT}/v4/stats" | jq . > "$OUT/stats.json"
curl -fsS "${AUTH[@]}" "localhost:${PORT}/v4/decodetrack?encodedTrack=$(enc "$SAMPLE")" | jq . > "$OUT/decodetrack.json"
# loadtracks（youtube-source プラグインが入っている場合のみ意味のある値になる）
curl -fsS "${AUTH[@]}" "localhost:${PORT}/v4/loadtracks?identifier=$(enc 'ytsearch:never gonna give you up')" | jq . > "$OUT/loadtracks_search.json" || echo "(loadtracks 取得失敗: youtube-source 未導入かも)"

echo ">> done. captured:"
ls -1 "$OUT"
