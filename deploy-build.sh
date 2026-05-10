if [ "$1" == "" ]; then
	echo "Usage: deploy-build2 [all|client|server]"
	exit
fi


if [ "$1" == "all" ] || [ "$1" == "server" ]; then
	for i in $(seq 1 5); 
	do 
		echo "Deploying to gluster$i"; 
		ssh root@gluster$i systemctl stop dfs-server; 
		scp target/release/dfs-server target/release/dfs-admin target/release/dfs-client root@gluster$i:/usr/bin/; 
		ssh root@gluster$i systemctl start dfs-server; 
		sleep 3; 
		echo ""; 
	done
fi


if [ "$1" == "all" ] || [ "$1" == "client" ]; then
	for i in nanopir3 rock5b
	do
		echo "Updating $i"
		ssh root@$i "podman stop dvr; systemctl stop dfs-client"
		scp target/release/dfs-client root@$i:/usr/bin/
		ssh root@$i "systemctl start dfs-client; sleep 6; podman start dvr"
		echo ""
	done
fi
