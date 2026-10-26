use std::fs;
use std::time::{Duration, Instant};

use shipcheck::runner::RunRequest;
use shipcheck::runner::act::{Act, Engine};
use shipcheck::runner::colima::Colima;

#[test]
fn a_colima_job_that_waits_past_its_time_for_the_vm_times_out() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    // SAFETY: this is the only test in this binary
    unsafe { std::env::set_var("SHIPCHECK_HOME", &home) };
    fs::create_dir_all(home.join("locks")).unwrap();
    // Another colima job holds the one VM
    let held = fs::File::create(home.join("locks/colima.lock")).unwrap();
    held.lock().unwrap();

    let run_dir = tmp.path().join("run");
    fs::create_dir_all(&run_dir).unwrap();
    let workflow = tmp.path().join("ci.yml");
    fs::write(
        &workflow,
        "on: push\njobs:\n  e2e:\n    runs-on: ubuntu-latest\n    steps:\n      - run: true\n",
    )
    .unwrap();
    let log = tmp.path().join("e2e.log");
    let needs = Default::default();
    let request = RunRequest {
        run_dir: &run_dir,
        file: "ci.yml",
        workflow: &workflow,
        job: "e2e",
        needs: &needs,
        artifacts: tmp.path(),
        repo_id: "acme/app",
        timeout: Duration::from_secs(1),
        shard: false,
        on_base: false,
        slot_lock: None,
    };
    let started = Instant::now();
    let outcome = Colima.run(&Act::default(), &request, &log).unwrap();
    assert_eq!(outcome.exit, None, "it timed out");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        fs::read_to_string(&log)
            .unwrap()
            .contains("timed out waiting for another colima job's VM")
    );

    // A job that passes artifacts is refused, rather than lose them in its VM
    fs::write(&workflow, "on: push\njobs:\n  e2e:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/upload-artifact@v4\n").unwrap();
    let Err(err) = Colima.run(&Act::default(), &request, &log) else {
        panic!("a colima job with artifacts ran");
    };
    assert!(err.contains("cannot pass artifacts"), "{err}");
}
