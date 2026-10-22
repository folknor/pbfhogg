//! Own process: this test deliberately lowers its soft descriptor limit.
#[test]
fn no_process_actions_leave_rlimit_nofile_unchanged() {
    use pbfhogg::host::{Host, MachineProbe, NoProcessActions};
    use std::sync::Arc;
    let dir = tempfile::tempdir().expect("directory");
    // SAFETY: this standalone test owns its process and initializes the output.
    unsafe {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit), 0);
        assert!(limit.rlim_max > 1024, "hard RLIMIT_NOFILE must exceed 1024");
        limit.rlim_cur = 1024;
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &limit), 0);
        let host = Host {
            diagnostics: pbfhogg::host::Diagnostics::discard(),
            process: Arc::new(NoProcessActions),
            resources: Arc::new(MachineProbe),
            page_cache: pbfhogg::host::PageCachePolicy::Keep,
            executor: pbfhogg::host::Executor::Ambient,
            scratch_parent: None,
        };
        pbfhogg::altw::external::external_join(
            std::path::Path::new("tests/fixtures/host_identity/inputs/base.osm.pbf"),
            &dir.path().join("external.pbf"),
            true,
            Default::default(),
            false,
            false,
            &Default::default(),
            false,
            &host,
        )
        .expect("external");
        let mut after = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut after), 0);
        assert_eq!((after.rlim_cur, after.rlim_max), (1024, limit.rlim_max));
    }
}
