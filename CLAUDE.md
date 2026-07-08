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
Do not push code until we verify that we do not have regressions.  Our test script is ./scripts/test_local_suite.sh
While it is important to make our code robust, it's imperative that we prioritize resolving the root cause of any problems.  When we identify a new problem that is difficult to resolve, we should attempt to create a local test to reproduce it so that we can easily validate when our solution works.


## Build Process
Always redirect the output of a build into a log file so that you can grep for errors if any occur.  This way you don't need to re-run a failed build just to get the errors.

## Local test suite

Run with: `bash scripts/test_local_suite.sh`

The suite runs 5-node local cluster on ports 8900–8904, mounts at /tmp/dfs-mount, and exercises
T1–T22. Logs go to /tmp/dfs-test-logs/. The client runs at **debug** log level so all events are
captured. Per-test snapshots are written to T<N>.log (e.g. T7.log) so each test's log is isolated.
When we create a new test, we should run it in an isolated manner and reproduce it, first, without the fixes so that we know if the fixes actually work.
Pipe the output of the test suite to a log file and print the log file path to the user so that they can monitor while the tests run.

### Reading logs effectively

- `grep " INFO\| ERROR\| WARN" /tmp/dfs-test-logs/T<N>.log` — info-level summary, same as before
- `grep -v "^ *\[.*DEBUG" /tmp/dfs-test-logs/T<N>.log` — strip debug lines, keep info/warn/error
- Full debug log is always there when you need to investigate a race or cache miss

### dfs_sync

`dfs_sync` in the test script calls `sync $MOUNT`, which triggers `fsync(ino=1)` → `fsyncdir` on
our FUSE handler. This blocks until all write buffers are flushed and metadata is committed.
Use it between test steps that write then read, instead of arbitrary `sleep` calls.
