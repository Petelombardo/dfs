This is a distributed filesystem.  It's intended to be fast, reliable, and highly available.  It runs via fuse.
All dev work is done locally, on this machine.  We spawn off 3 dfs-server instances on different ports, and then run the dfs-client to mount the storage and test from.
When dev is stable and confirmed to work, we can test on staging.
Always ask before touching staging.
Staging has 6 servers - 5 storage nodes, and 1 client.
* root@nanopir3 is the client; The cluster is mounted at /mnt/test
* root@gluster1 root@gluster2 root@gluster3 root@gluster4 root@gluster5 - these are the storage nodes.  The data, metadata, and config folders are located at /mnt/gluster/dfs/...
Our primary objectives are reliability (high availability and redundancy), and speed.  We want to maximize both.
You can ssh into the staging nodes to test and gather logs.  All staging nodes start the dfs services using systemd.
We run one service in staging, a hdhomerun dvr.  It runs from /mnt/test/podman/dvr/podman-compose.yml 
Band-aids to the code are a last-resort.  We should always try to fix the underlying problem at its root.
Propose protocol updates if they will make our app more efficient.
