use pi_ext::{mvp_conformance_cases, Hostcall};
use pi_permissions::Capability;

#[test]
fn hostcalls_map_to_capabilities() {
    let http = Hostcall::Http {
        method: "GET".to_string(),
        url: "https://example.com".to_string(),
    };
    assert_eq!(http.required_capability(), Capability::Network);

    let cases = mvp_conformance_cases();
    assert!(cases.len() >= 2);
    for case in cases {
        assert_eq!(
            case.hostcall.required_capability(),
            case.expected_capability
        );
    }
}
