#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

# Download profiling region datasets from Geofabrik.
# Each region gets a PBF + daily diff (OSC).
#
# The latest PBF already includes the current diff's changes. This is fine
# for profiling: rewrite fractions and per-element costs are identical
# regardless of whether the diff content is "new". The merge output is
# semantically wrong but we only measure performance, not correctness.

DATADIR="data"

download() {
    local dest="$1"
    local url="$2"
    if [ -f "$dest" ]; then
        echo "  SKIP (exists): $dest"
        return
    fi
    echo "  GET: $url"
    echo "    -> $dest"
    curl -L --progress-bar -o "$dest" "$url"
}

echo "=== Malta (8 MB) ==="
download "$DATADIR/malta-20260225-seq4706.osm.pbf" \
    "https://download.geofabrik.de/europe/malta-latest.osm.pbf"
download "$DATADIR/malta-20260225-seq4706.osc.gz" \
    "https://download.geofabrik.de/europe/malta-updates/000/004/706.osc.gz"

echo ""
echo "=== Greater London (116 MB) ==="
download "$DATADIR/greater-london-20260225-seq4704.osm.pbf" \
    "https://download.geofabrik.de/europe/united-kingdom/england/greater-london-latest.osm.pbf"
download "$DATADIR/greater-london-20260225-seq4704.osc.gz" \
    "https://download.geofabrik.de/europe/united-kingdom/england/greater-london-updates/000/004/704.osc.gz"

echo ""
echo "=== Switzerland (500 MB) ==="
download "$DATADIR/switzerland-20260225-seq4707.osm.pbf" \
    "https://download.geofabrik.de/europe/switzerland-latest.osm.pbf"
download "$DATADIR/switzerland-20260225-seq4707.osc.gz" \
    "https://download.geofabrik.de/europe/switzerland-updates/000/004/707.osc.gz"

echo ""
echo "=== Norway (1.3 GB) ==="
download "$DATADIR/norway-20260225-seq4709.osm.pbf" \
    "https://download.geofabrik.de/europe/norway-latest.osm.pbf"
download "$DATADIR/norway-20260225-seq4709.osc.gz" \
    "https://download.geofabrik.de/europe/norway-updates/000/004/709.osc.gz"

echo ""
echo "=== Japan (2.2 GB) ==="
download "$DATADIR/japan-20260225-seq4706.osm.pbf" \
    "https://download.geofabrik.de/asia/japan-latest.osm.pbf"
download "$DATADIR/japan-20260225-seq4706.osc.gz" \
    "https://download.geofabrik.de/asia/japan-updates/000/004/706.osc.gz"

echo ""
echo "=== Downloads complete ==="
echo ""
echo "To profile all regions:"
echo "  scripts/profile-region.sh malta data/malta-20260225-seq4706.osm.pbf data/malta-20260225-seq4706.osc.gz"
echo "  scripts/profile-region.sh greater-london data/greater-london-20260225-seq4704.osm.pbf data/greater-london-20260225-seq4704.osc.gz"
echo "  scripts/profile-region.sh switzerland data/switzerland-20260225-seq4707.osm.pbf data/switzerland-20260225-seq4707.osc.gz"
echo "  scripts/profile-region.sh norway data/norway-20260225-seq4709.osm.pbf data/norway-20260225-seq4709.osc.gz"
echo "  scripts/profile-region.sh japan data/japan-20260225-seq4706.osm.pbf data/japan-20260225-seq4706.osc.gz"
