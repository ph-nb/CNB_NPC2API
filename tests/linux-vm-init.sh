#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mkdir -p /tmp
/bin/busybox ip link set lo up
/work/cnb-npc2api-tests --test-threads=1
result=$?
echo CNB_NPC2API_LINUX_RESULT=$result
echo MIERU_LINUX_RESULT=$result
/bin/busybox poweroff -f
