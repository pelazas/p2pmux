use std::path::PathBuf;

use p2pmux::config::{load_from, save_to, validate_display_name};

fn temp_config_path() -> PathBuf {
    std::env::temp_dir()
        .join(format!("p2pmux-config-test-{}", std::process::id()))
        .join("config.toml")
}

#[test]
fn display_name_config_round_trips() {
    let path = temp_config_path();
    save_to(&path, "  pelazas  ").expect("save config");
    assert_eq!(
        load_from(&path).expect("load config"),
        Some("pelazas".into())
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp parent"));
}

#[test]
fn display_name_validation_rejects_empty_long_and_control_values() {
    assert!(validate_display_name("  ").is_err());
    assert!(validate_display_name(&"x".repeat(33)).is_err());
    assert!(validate_display_name("bad\nname").is_err());
}
