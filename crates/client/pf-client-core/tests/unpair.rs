//! `orchestrate::unpair_host` against a scratch config dir. One test, alone in its binary:
//! it points this process's config and cache dirs at the scratch dir.
#![cfg(any(target_os = "linux", windows))]

use pf_client_core::orchestrate;
use pf_client_core::trust::{KnownHosts, Settings};

/// Unpairing keeps the record and takes what its pin keyed: the cached game list, and a
/// default pointing at the record. Every shell unpairs through this one function.
#[test]
fn unpairing_keeps_the_record_and_clears_what_its_pin_keyed() {
    let home = std::env::temp_dir().join(format!("pf-core-unpair-{}", std::process::id()));
    let store = home.join("config");
    let fp = "ab".repeat(32);
    let catalog = home.join(if cfg!(windows) {
        "punktfunk/cache/library"
    } else {
        "punktfunk/library"
    });
    std::fs::create_dir_all(&store).expect("scratch config dir");
    std::fs::create_dir_all(&catalog).expect("scratch cache dir");
    std::fs::write(
        store.join("client-known-hosts.json"),
        format!(
            r#"{{"hosts":[{{"name":"Desk","addr":"10.0.0.5","port":9777,"fp_hex":"{fp}","paired":true,"id":"rec-1"}}]}}"#
        ),
    )
    .expect("seed the store");
    let settings_file = store.join(if cfg!(windows) {
        "client-windows-settings.json"
    } else {
        "client-gtk-settings.json"
    });
    std::fs::write(&settings_file, r#"{"default_host":"rec-1"}"#).expect("seed the default");
    let cached = catalog.join(format!("{fp}.json"));
    std::fs::write(&cached, r#"{"games":[],"fetched_at":0}"#).expect("seed the catalog");
    // SAFETY: the only test in this binary, so no other thread reads the environment.
    unsafe {
        std::env::set_var("PUNKTFUNK_CONFIG_DIR", &store);
        std::env::set_var("XDG_CACHE_HOME", &home);
        std::env::set_var("LOCALAPPDATA", &home);
    }

    let by_name = |known: &KnownHosts| known.hosts.iter().position(|h| h.name == "Desk");
    let host = orchestrate::unpair_host(by_name)
        .expect("the store saves")
        .expect("the record is there");
    assert!(host.fp_hex.is_empty() && !host.paired);

    let known = KnownHosts::read();
    assert_eq!(known.hosts.len(), 1, "the record stays");
    assert_eq!(known.hosts[0].id.as_deref(), Some("rec-1"));
    assert!(known.hosts[0].fp_hex.is_empty() && !known.hosts[0].paired);
    assert_eq!(
        Settings::load().default_host,
        None,
        "an unpaired host is no landing"
    );
    assert!(!cached.exists(), "the cached catalog outlived the pin");

    // A pick that names nothing writes nothing.
    let before = std::fs::read(store.join("client-known-hosts.json")).expect("the store");
    assert!(orchestrate::unpair_host(|_| None)
        .expect("no write")
        .is_none());
    assert_eq!(
        std::fs::read(store.join("client-known-hosts.json")).expect("the store"),
        before
    );

    std::fs::remove_dir_all(&home).ok();
}
