if [ "$1" == "" ]; then
	echo "Usage: deploy-build.sh [all|client|server]"
	echo ""
	echo "  server  - rolling update of all 5 storage nodes (safe for non-protocol changes)"
	echo "  client  - update client nodes (nanopir3, rock5b)"
	echo "  all     - stop clients first, rolling update servers, then restart clients"
	echo "            (correct order for protocol-changing deploys)"
	exit
fi

# For 'all', stop clients before touching servers so the old client binary
# is never talking to a mix of old and new servers during the rolling window.
if [ "$1" == "all" ]; then
	for i in nanopir3 rock5b
	do
		echo "Stopping $i"
		ssh root@$i "podman stop dvr; systemctl stop dfs-client"
	done
	echo ""
fi

if [ "$1" == "all" ] || [ "$1" == "server" ]; then
	for i in gluster2 gluster3 gluster4 gluster5 gluster1;
	do
		echo "Deploying to $i";
		ssh root@$i systemctl stop dfs-server;
		sleep 1
		scp target/release/dfs-server target/release/dfs-admin target/release/dfs-client root@$i:/usr/bin/;
		ssh root@$i systemctl start dfs-server;
		sleep 3;
		echo "";
	done
fi

echo "Waiting a few seconds for the servers to settle"
sleep 3

if [ "$1" == "all" ] || [ "$1" == "client" ]; then
	for i in nanopir3 rock5b
	do
		echo "Updating $i"
		ssh root@$i "systemctl stop dfs-client"
		sleep 1
		scp target/release/dfs-client root@$i:/usr/bin/
		ssh root@$i "systemctl start dfs-client; sleep 6; podman start dvr"
		echo ""
	done
fi
