#!/bin/sh
# distroproof's `command` for distroproof-vertical.toml: the vertical server's
# requests with their data file on a device that delays every read by
# DELAY_MS, below the filesystem: ext4 over dm-delay over a loop device.
# Two requests, one reading nothing and one reading READS pages. The session
# is examples/vertical-one-request.yml with the server, the file and the plan
# replaced; its document and the programs' own clocks come back as
# vertical-guest.json and vertical-guest.err. Everything made is taken down.
set -u
DELAY_MS=20
READS=16
omn=/opt/omn/bin/omn
server=$PWD/target/dp-vertical/$(uname -m)/server
mnt=/mnt/omn-delayed
image=$PWD/delayed.img
name=omn-vertical-delay

modprobe dm_delay 2>/dev/null
truncate -s 256M "$image"
loop=$(losetup --show -f "$image") || { echo "vertical: no loop device"; exit 1; }
sectors=$(blockdev --getsz "$loop")
# Reads delayed DELAY_MS, writes not: the server writes its file once, and
# only its reads are the case.
dmsetup create "$name" --table "0 $sectors delay $loop 0 $DELAY_MS $loop 0 0" || {
    echo "vertical: dmsetup create failed"; losetup -d "$loop"; exit 1; }
mkfs.ext4 -q "/dev/mapper/$name"
mkdir -p "$mnt"
mount "/dev/mapper/$name" "$mnt"
echo "vertical: data on $mnt (dm-delay ${DELAY_MS} ms reads over $loop)"

sed -e "s|target/omn-vertical/server|$server|" \
    -e "s|target/omn-vertical/data|$mnt/data|" \
    -e '/^        - "40:0"$/,/^        - "0:0:0:131072"$/c\        - "0:0"\n        - "0:'"$READS"'"' \
    examples/vertical-one-request.yml > session.yml
grep -A4 'launch:' session.yml
"$omn" -f session.yml --format json > vertical-guest.json 2> vertical-guest.err
echo "vertical: omn exited $?"
grep '^vertical: role=worker req' vertical-guest.err

umount "$mnt"
dmsetup remove --retry "$name"
losetup -d "$loop"
rm -f "$image"
