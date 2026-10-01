const CANONICAL_V1: &str = include_str!("../wit/kinetix-plugin.wit");

#[test]
fn versioned_adapter_worlds_use_the_canonical_v1_host_interfaces() {
    assert_eq!(
        include_str!("../wit/v2/deps/kinetix-plugin/kinetix-plugin.wit"),
        CANONICAL_V1
    );
    assert_eq!(
        include_str!("../wit/v3/deps/kinetix-plugin/kinetix-plugin.wit"),
        CANONICAL_V1
    );
}
