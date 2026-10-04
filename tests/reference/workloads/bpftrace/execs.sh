#!/bin/sh
# Exec /bin/true N times (default 20): execsnoop and proc.exec both count
# exactly N exec events whose new comm is `true`.
n=${1:-20}
i=0
while [ "$i" -lt "$n" ]; do
	/bin/true
	i=$((i + 1))
done
