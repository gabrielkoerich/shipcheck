use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use shipcheck::config::Engine;
use shipcheck::runner::Runner;
use shipcheck::runner::act::Act;

#[test]
fn a_colima_job_puts_colima_and_lima_in_the_key() {
    let dir = tempfile::tempdir().unwrap();

    for (tool, out) in [
        ("act", "act version 0.2.89"),
        ("colima", "colima version 0.10.3\ngit commit: abc"),
        ("limactl", "limactl version 2.2.1"),
    ] {
        let path = dir.path().join(tool);
        fs::write(&path, format!("#!/bin/sh\nprintf '{out}\\n'\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("PATH", format!("{}:/usr/bin:/bin", dir.path().display())) };
    assert_eq!(Act::default().identity().unwrap(), "act version 0.2.89");
    let vm = Act::default().with_engines(BTreeMap::from([("ci.yml/e2e".into(), Engine::Colima)]));
    let id = vm.identity().unwrap();
    let (head, setup) = id.rsplit_once(", setup ").unwrap();
    assert_eq!(
        head,
        "act version 0.2.89\0colima\0colima version 0.10.3, limactl version 2.2.1"
    );
    assert_eq!(setup.len(), 16);
    fs::write(
        dir.path().join("limactl"),
        "#!/bin/sh\nprintf 'limactl version 2.2.2\\n'\n",
    )
    .unwrap();
    assert_ne!(
        vm.identity().unwrap(),
        id,
        "a lima upgrade runs the job again"
    );
}
