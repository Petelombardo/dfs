if [ "$1" == "" ]; then
	echo "Usage: deploy-build2 [all|client|server]"
	exit
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
		ssh root@$i "podman stop dvr; systemctl stop dfs-client"
		sleep 1
		scp target/release/dfs-client root@$i:/usr/bin/
		ssh root@$i "systemctl start dfs-client; sleep 6; podman start dvr"
		echo ""
	done
fi
