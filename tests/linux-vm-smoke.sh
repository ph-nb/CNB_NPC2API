#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox ip link set lo up
result=0
/work/cnb-forge --help || result=1
/work/cnb-forge --config /work/config.yaml &
pid=$!
health=""
for n in 1 2 3 4 5 6 7 8 9 10; do
    health=$(/bin/busybox wget -qO- http://127.0.0.1:18080/healthz 2>/dev/null) && break
    /bin/busybox sleep 1
done
case "$health" in *'"status":"ok"'*) echo "Release health: ok" ;; *) echo "Release health: failed"; result=1 ;; esac
models=$(/bin/busybox wget -qO- http://127.0.0.1:18080/v1/models 2>/dev/null)
case "$models" in *'"id":"test-model"'*) echo "Release models: ok" ;; *) echo "Release models: failed"; result=1 ;; esac
kill "$pid" 2>/dev/null
wait "$pid" 2>/dev/null
echo CNB_FORGE_RELEASE_RESULT=$result
echo MIERU_LINUX_RESULT=$result
/bin/busybox poweroff -f
