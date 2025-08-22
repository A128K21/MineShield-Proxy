#[test]
fn run_integration_harness() {
    let status = std::process::Command::new("bash")
        .arg("test-harness/run.sh")
        .env("BOT_COUNT", "2")
        .status()
        .expect("failed to run test harness");
    assert!(status.success());
}
