#!/busybox/sh
export RUST_BACKTRACE=1
/test_workload /workloads/sqlite_sanity.toml
