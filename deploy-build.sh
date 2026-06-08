if [ "$1" == "" ]; then
	echo "Usage: deploy-build.sh [all|client|server]"
	echo ""
	echo "  server  - rolling update of all 5 storage nodes (safe for non-protocol changes)"
	echo "  client  - update client nodes (nanopir3, rock5b)"
	echo "  all     - stop clients first, rolling update servers, then restart clients"
	echo "            (correct order for protocol-changing deploys)"
	exit
fi

# get_dfs_mountpoint <host>
# Prints the DFS mount point from the host's systemd service file.
get_dfs_mountpoint() {
	local host="$1"
	local mp
	mp=$(ssh root@"$host" "grep -oP '(?<=mount \")([^\"]+)' /etc/systemd/system/dfs-client.service 2>/dev/null | head -1")
	echo "${mp:-/mnt/test}"
}

# get_dfs_containers <host> <mountpoint>
# Prints names of running containers that have a bind-mount under <mountpoint>.
get_dfs_containers() {
	local host="$1"
	local mountpoint="$2"
	ssh root@"$host" "
		podman ps --format '{{.Names}}' 2>/dev/null | while read name; do
			mounts=\$(podman inspect \"\$name\" --format '{{range .Mounts}}{{.Source}} {{end}}' 2>/dev/null)
			if echo \"\$mounts\" | grep -q '$mountpoint'; then
				echo \"\$name\"
			fi
		done
	"
}

# stop_dfs_client <host> <containers_space_separated>
# Stops given containers then stops dfs-client.
stop_dfs_client() {
	local host="$1"
	local containers="$2"
	if [ -n "$containers" ]; then
		echo "  [$host] Stopping containers: $containers"
		ssh root@"$host" "echo '$containers' | xargs podman stop 2>/dev/null"
	fi
	echo "  [$host] Stopping dfs-client"
	ssh root@"$host" "systemctl stop dfs-client"
}

# deploy_dfs_client <host> <containers_to_restart>
# Copies binary, starts dfs-client, waits for warmup, restarts containers.
deploy_dfs_client() {
	local host="$1"
	local containers="$2"
	local mountpoint
	mountpoint=$(get_dfs_mountpoint "$host")

	echo "  [$host] Copying binary"
	scp target/release/dfs-client root@"$host":/usr/bin/

	echo "  [$host] Starting dfs-client (init() blocks until metadata warmup)"
	ssh root@"$host" "systemctl start dfs-client"

	echo "  [$host] Waiting for $mountpoint to be accessible"
	ssh root@"$host" "until ls '$mountpoint' >/dev/null 2>&1; do sleep 0.5; done"
	echo "  [$host] Cache warm — mount ready"

	if [ -n "$containers" ]; then
		echo "  [$host] Restarting containers: $containers"
		ssh root@"$host" "echo '$containers' | xargs podman start 2>/dev/null"
	fi
}

# ─── all: stop clients first so old binary never talks to a mixed-version cluster
if [ "$1" == "all" ]; then
	for i in nanopir3 rock5b; do
		echo "--- Stopping $i ---"
		mp=$(get_dfs_mountpoint "$i")
		containers=$(get_dfs_containers "$i" "$mp")
		stop_dfs_client "$i" "$containers"
		# Stash container list in a per-host variable for the restart phase.
		eval "containers_${i//-/_}='$containers'"
		echo ""
	done
	echo ""
fi

# ─── server rolling update ────────────────────────────────────────────────────
if [ "$1" == "all" ] || [ "$1" == "server" ]; then
	for i in gluster2 gluster3 gluster4 gluster5 gluster1; do
		echo "Deploying to $i"
		ssh root@$i systemctl stop dfs-server
		sleep 1
		scp target/release/dfs-server target/release/dfs-admin target/release/dfs-client root@$i:/usr/bin/
		ssh root@$i systemctl start dfs-server
		sleep 3
		echo ""
	done
fi

echo "Waiting a few seconds for the servers to settle"
sleep 3

# ─── client update ────────────────────────────────────────────────────────────
if [ "$1" == "all" ] || [ "$1" == "client" ]; then
	for i in nanopir3 rock5b; do
		echo "=== Deploying client to $i ==="

		if [ "$1" == "client" ]; then
			# client-only mode: discover and stop containers now.
			mp=$(get_dfs_mountpoint "$i")
			containers=$(get_dfs_containers "$i" "$mp")
			stop_dfs_client "$i" "$containers"
			sleep 1
		else
			# all mode: containers already stopped above; retrieve stashed list.
			varname="containers_${i//-/_}"
			containers="${!varname}"
			sleep 1
		fi

		deploy_dfs_client "$i" "$containers"
		echo ""
	done
fi
