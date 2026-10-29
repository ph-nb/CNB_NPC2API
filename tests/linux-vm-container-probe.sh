#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox ip link set lo up
echo "Kernel: $(/bin/busybox uname -r)"
for name in overlay cgroup2; do
    if /bin/busybox grep -qw "$name" /proc/filesystems; then
        echo "$name filesystem: available"
    else
        echo "$name filesystem: unavailable"
    fi
done
if /bin/busybox unshare -m -p -f /bin/busybox true 2>/dev/null; then
    echo "Mount and PID namespaces: available"
else
    echo "Mount and PID namespaces: unavailable"
fi
if /bin/busybox mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null; then
    echo "cgroup2 mount: available"
else
    echo "cgroup2 mount: unavailable"
fi
echo MIERU_LINUX_RESULT=0
/bin/busybox poweroff -f
