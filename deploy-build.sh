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

# resolve_ip <host>
# dfs-admin needs a numeric address, not a hostname.
resolve_ip() {
	getent hosts "$1" | awk '{print $1; exit}'
}

# backup_remote_binaries <host> <binary> [binary ...]
# Copies each /usr/bin/<binary> to /usr/bin/<binary>.old on the remote host
# before it gets overwritten by the new build, so a bad deploy can be rolled
# back with `cp /usr/bin/<binary>.old /usr/bin/<binary>` + a service restart
# instead of needing a fresh build. Overwrites any previous .old — it's meant
# to hold the immediately-prior version, not a longer history.
backup_remote_binaries() {
	local host="$1"; shift
	for bin in "$@"; do
		echo "  [$host] Backing up /usr/bin/$bin -> /usr/bin/$bin.old"
		ssh root@"$host" "[ -f /usr/bin/$bin ] && cp -f /usr/bin/$bin /usr/bin/$bin.old || true"
	done
}

# wait_for_convergence <ip> <baseline_count> <label>
# Polls the given node's own file list until it reports at least baseline_count
# files, or gives up after ~90s. A node that just restarted may not yet have
# caught up on metadata written to the rest of the cluster while it was down —
# see the staging incident this replaces a fixed `sleep` for: dvr.conf's create()
# asks the server before minting a new file identity, but that check is only as
# good as whichever node answers it. A fixed sleep here was a guess about how
# long that catch-up takes; this checks the actual condition instead.
wait_for_convergence() {
	local ip="$1" baseline="$2" label="$3"
	local count=0
	for _ in $(seq 1 45); do
		count=$(target/release/dfs-admin --cluster "${ip}:8900" --format json file list 2>/dev/null \
			| python3 -c "import json,sys; print(json.load(sys.stdin).get('total_count', 0))" 2>/dev/null || echo 0)
		if [ "$count" -ge "$baseline" ] 2>/dev/null; then
			echo "  [$label] Converged: $count files (baseline $baseline)"
			return 0
		fi
		sleep 2
	done
	echo "  [$label] WARNING: only $count/$baseline files after ~90s — this node may still be catching up on metadata. Proceeding anyway, but watch for it."
	return 1
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

	backup_remote_binaries "$host" dfs-client

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

# Client hosts, defined once so the stop phase (below) and the deploy phase
# (further down) can never drift out of sync — an earlier version listed
# 10.25.1.80 in the stop loop but not the deploy loop, which would stop that
# node and never bring it back up on an `all` run.
CLIENT_HOSTS="nanopir3 rock5b 10.25.1.80"

# sanitize_varname <host> — hostnames/IPs used as shell variable names must
# contain only [A-Za-z0-9_]. Replace both '.' (IPs) and '-' (hostnames) with '_'.
# ${i//-/_} alone left the dots in an IP, producing an invalid name.
sanitize_varname() { echo "containers_${1//[.-]/_}"; }

# ─── all: stop clients first so old binary never talks to a mixed-version cluster
if [ "$1" == "all" ]; then
	for i in $CLIENT_HOSTS; do
		echo "--- Stopping $i ---"
		mp=$(get_dfs_mountpoint "$i")
		containers=$(get_dfs_containers "$i" "$mp")
		stop_dfs_client "$i" "$containers"
		# Stash container list in a per-host variable for the restart phase.
		eval "$(sanitize_varname "$i")='$containers'"
		echo ""
	done
	echo ""
	sleep 3
fi

# ─── server rolling update ────────────────────────────────────────────────────
if [ "$1" == "all" ] || [ "$1" == "server" ]; then
	# Baseline file count from whichever node answers first — every node we
	# restart below must catch back up to at least this before we move on to the
	# next one (or, after the last one, to restarting clients). Query before
	# touching anything so a mid-restart node can't be the one we ask.
	baseline_ip=$(resolve_ip gluster1)
	baseline_count=$(target/release/dfs-admin --cluster "${baseline_ip}:8900" --format json file list 2>/dev/null \
		| python3 -c "import json,sys; print(json.load(sys.stdin).get('total_count', 0))" 2>/dev/null || echo 0)
	echo "Baseline file count before server deploy: $baseline_count"
	echo ""

	for i in gluster2 gluster3 gluster4 gluster5 gluster1; do
		echo "Deploying to $i"
		ssh root@$i systemctl stop dfs-server
		sleep 1
		backup_remote_binaries "$i" dfs-server dfs-admin dfs-client
		scp target/release/dfs-server target/release/dfs-admin target/release/dfs-client root@$i:/usr/bin/
		ssh root@$i systemctl start dfs-server
		wait_for_convergence "$(resolve_ip $i)" "$baseline_count" "$i"
		echo ""
	done
fi

# ─── client update ────────────────────────────────────────────────────────────
if [ "$1" == "all" ] || [ "$1" == "client" ]; then
	for i in $CLIENT_HOSTS; do
		echo "=== Deploying client to $i ==="

		if [ "$1" == "client" ]; then
			# client-only mode: discover and stop containers now.
			mp=$(get_dfs_mountpoint "$i")
			containers=$(get_dfs_containers "$i" "$mp")
			stop_dfs_client "$i" "$containers"
			sleep 1
		else
			# all mode: containers already stopped above; retrieve stashed list.
			varname="$(sanitize_varname "$i")"
			containers="${!varname}"
			sleep 1
		fi

		deploy_dfs_client "$i" "$containers"
		echo ""
	done
fi
