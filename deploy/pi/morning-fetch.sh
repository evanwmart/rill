#!/usr/bin/env bash
# The morning board's bridge: fetch the forecast for the site in morning.toml
# and drop it where the signage server reads it. The server has no outbound
# network of its own; this is the one thing that does.
#
#   deploy/pi/morning-fetch.sh [DATA_DIR]      # default ~/.local/share/rill-signage/data
#
# Run it every 15 minutes (cron: */15 * * * * ~/deploy/morning-fetch.sh), or
# once by hand. Writes weather.json atomically; a failed fetch leaves the
# previous file in place, and the page says how old it is.
set -eu
DATA=${1:-$HOME/.local/share/rill-signage/data}
CONF="$DATA/morning.toml"
lat=$(sed -n 's/^latitude *= *\([-0-9.]*\).*/\1/p' "$CONF" | head -1)
lon=$(sed -n 's/^longitude *= *\([-0-9.]*\).*/\1/p' "$CONF" | head -1)
[ -n "$lat" ] && [ -n "$lon" ] || { echo "no latitude/longitude in $CONF" >&2; exit 2; }
url="https://api.open-meteo.com/v1/forecast?latitude=$lat&longitude=$lon"
url="$url&current=temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,is_day,wind_speed_10m"
url="$url&hourly=temperature_2m,precipitation,weather_code,cloud_cover,visibility"
url="$url&daily=temperature_2m_max,temperature_2m_min,sunrise,sunset"
url="$url&timezone=auto&forecast_days=3"
tmp="$DATA/weather.json.tmp"
if curl -sfL --max-time 30 "$url" -o "$tmp" && [ -s "$tmp" ]; then
  mv "$tmp" "$DATA/weather.json"
  echo "weather.json updated ($(date +%H:%M))"
else
  rm -f "$tmp"
  echo "fetch failed; keeping the previous weather.json" >&2
  exit 1
fi
