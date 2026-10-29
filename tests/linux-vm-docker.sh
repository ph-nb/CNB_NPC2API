#!/bin/sh
if [ ! -f /work/.switched ]; then
    /bin/busybox mkdir -p /newroot
    /bin/busybox mount -t tmpfs tmpfs /newroot
    /bin/busybox mkdir -p /newroot/bin /newroot/lib /newroot/work /newroot/proc /newroot/sys /newroot/dev /newroot/tmp /newroot/run
    /bin/busybox cp /bin/busybox /newroot/bin/busybox
    /bin/busybox cp -a /lib/. /newroot/lib/
    /bin/busybox ln -s busybox /newroot/bin/sh
    /bin/busybox cp -a /work/. /newroot/work/
    /bin/busybox cp /audit-init /newroot/init
    /bin/busybox touch /newroot/work/.switched
    exec /bin/busybox switch_root /newroot /init
fi
export PATH=/work:/bin:/sbin:/usr/bin:/usr/sbin
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mkdir -p /sys/fs/cgroup /tmp/docker-data /tmp/docker-exec /tmp/rootfs/bin /tmp/rootfs/lib /tmp/rootfs/app /tmp/rootfs/usr/local/bin
/bin/busybox mount -t cgroup2 cgroup2 /sys/fs/cgroup 2>/dev/null || true
/bin/busybox ip link set lo up
dockerd --host=unix:///tmp/docker.sock --data-root=/tmp/docker-data --exec-root=/tmp/docker-exec --pidfile=/tmp/docker.pid --iptables=false --ip6tables=false --bridge=none --userland-proxy=false --storage-driver=vfs --log-level=error >/tmp/dockerd.log 2>&1 &
daemon=$!
ready=0
for n in $(/bin/busybox seq 1 90); do
    if docker -H unix:///tmp/docker.sock info >/dev/null 2>&1; then ready=1; break; fi
    if ! kill -0 "$daemon" 2>/dev/null; then break; fi
    /bin/busybox sleep 1
done
result=0
if [ "$ready" -ne 1 ]; then
    echo "Docker daemon: failed"
    /bin/busybox tail -n 30 /tmp/dockerd.log
    result=1
else
    echo "Docker daemon: ready (vfs)"
    /bin/busybox cp /bin/busybox /tmp/rootfs/bin/busybox
    /bin/busybox cp -a /lib/. /tmp/rootfs/lib/
    /bin/busybox cp /work/cnb-forge /tmp/rootfs/usr/local/bin/cnb-forge
    /bin/busybox cp /work/config.yaml /tmp/rootfs/app/config.yaml
    /bin/busybox tar -C /tmp/rootfs -cf /tmp/rootfs.tar .
    if docker -H unix:///tmp/docker.sock import /tmp/rootfs.tar cnb-forge-smoke:local >/dev/null 2>&1 &&
       docker -H unix:///tmp/docker.sock run -d --network none --name cnb-forge-smoke cnb-forge-smoke:local /usr/local/bin/cnb-forge --config /app/config.yaml >/dev/null 2>&1; then
        health=""
        for n in $(/bin/busybox seq 1 30); do
            health=$(docker -H unix:///tmp/docker.sock exec cnb-forge-smoke /bin/busybox wget -qO- http://127.0.0.1:18080/healthz 2>/dev/null) && break
            /bin/busybox sleep 1
        done
        case "$health" in *'"status":"ok"'*) echo "Docker container health: ok" ;; *) echo "Docker container health: failed"; result=1 ;; esac
        models=$(docker -H unix:///tmp/docker.sock exec cnb-forge-smoke /bin/busybox wget -qO- http://127.0.0.1:18080/v1/models 2>/dev/null)
        case "$models" in *'"id":"test-model"'*) echo "Docker container models: ok" ;; *) echo "Docker container models: failed"; result=1 ;; esac
    else
        echo "Docker image import or container start: failed"
        /bin/busybox tail -n 30 /tmp/dockerd.log
        result=1
    fi
    /bin/busybox mkdir -p /tmp/context/target/x86_64-unknown-linux-musl/release
    /bin/busybox cp /work/cnb-forge /tmp/context/target/x86_64-unknown-linux-musl/release/cnb-forge
    /bin/busybox cp /work/config.yaml /tmp/context/config.example.yaml
    /bin/busybox cp /work/Dockerfile.musl /tmp/context/Dockerfile.musl
    /bin/busybox cp /work/Dockerfile.musl.dockerignore /tmp/context/Dockerfile.musl.dockerignore
    /bin/busybox cp /work/dockerignore /tmp/context/.dockerignore
    if DOCKER_BUILDKIT=0 docker -H unix:///tmp/docker.sock build -t cnb-forge-build:local -f /tmp/context/Dockerfile.musl /tmp/context >/tmp/docker-build.log 2>&1 &&
       docker -H unix:///tmp/docker.sock run -d --network host --name cnb-forge-build cnb-forge-build:local >/dev/null 2>&1; then
        health=""
        for n in $(/bin/busybox seq 1 30); do
            health=$(/bin/busybox wget -qO- http://127.0.0.1:18080/healthz 2>/dev/null) && break
            /bin/busybox sleep 1
        done
        case "$health" in *'"status":"ok"'*) echo "Dockerfile.musl build and health: ok" ;; *) echo "Dockerfile.musl container health: failed"; result=1 ;; esac
        models=$(/bin/busybox wget -qO- http://127.0.0.1:18080/v1/models 2>/dev/null)
        case "$models" in *'"id":"test-model"'*) echo "Dockerfile.musl models: ok" ;; *) echo "Dockerfile.musl models: failed"; result=1 ;; esac
    else
        echo "Dockerfile.musl build or container start: failed"
        /bin/busybox tail -n 30 /tmp/docker-build.log
        /bin/busybox tail -n 20 /tmp/dockerd.log
        result=1
    fi
fi
kill "$daemon" 2>/dev/null || true
echo "CNB_DOCKER_RESULT=$result"
echo "MIERU_LINUX_RESULT=$result"
/bin/busybox poweroff -f
