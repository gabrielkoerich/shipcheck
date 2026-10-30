use std::time::Duration;

/// `.cargo/config.toml` routes every request a test makes to a dead port, so nothing leaves the machine
#[test]
fn a_test_never_reaches_the_internet() {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .build()
        .into();
    let err = agent
        .get("https://api.github.com/zen")
        .call()
        .expect_err("a test reached api.github.com");
    let text = err.to_string().to_lowercase();
    assert!(
        text.contains("refused") || text.contains("proxy"),
        "the request failed some other way, not at the proxy: {err}"
    );
}
