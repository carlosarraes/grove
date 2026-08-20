use grove::exposure::Exposure;

#[test]
fn local_exposure_keeps_browser_and_bind_hosts_on_loopback() {
    let exposure = Exposure::local();

    assert!(!exposure.is_exposed());
    assert_eq!(exposure.public_host(), "localhost");
    assert_eq!(exposure.bind_host(), "127.0.0.1");
}

#[test]
fn explicit_exposure_uses_the_public_host_and_binds_every_interface() {
    for host in ["192.168.50.12", "dev-mac.local", "macbook.tailnet"] {
        let exposure = Exposure::explicit(host).expect("valid public host");

        assert!(exposure.is_exposed());
        assert_eq!(exposure.public_host(), host);
        assert_eq!(exposure.bind_host(), "0.0.0.0");
    }
}

#[test]
fn explicit_exposure_rejects_values_that_are_not_remote_ipv4_or_hostnames() {
    for invalid in [
        "",
        "localhost",
        "127.0.0.1",
        "0.0.0.0",
        "::1",
        "2001:db8::1",
        "http://dev-mac.local",
        "dev-mac.local:8080",
        "dev/mac",
        "dev mac",
        "-dev.local",
        "dev-.local",
    ] {
        let error = Exposure::explicit(invalid)
            .expect_err("local, wildcard, IPv6, or URL-shaped values must be rejected");
        assert!(
            error.to_string().contains(invalid) || invalid.is_empty(),
            "the error should name the rejected value {invalid:?}: {error:#}"
        );
    }
}
