//! Drop settings (D50).

use relay_engine::{Engine, EngineError};
use tempfile::TempDir;

#[test]
fn drops_default_on_into_pictures_and_follow_settings() {
    let home = TempDir::new().unwrap();
    let mut engine = Engine::init(home.path(), "desk").unwrap();
    let settings = engine.drop_settings().unwrap();
    assert!(settings.receive);
    assert!(!settings.inbox_set);
    assert_eq!(settings.inbox, relay_engine::default_drop_inbox());

    let inbox = TempDir::new().unwrap();
    let chosen = inbox.path().join("Drops");
    let settings = engine.set_drop_inbox(Some(&chosen)).unwrap();
    assert!(chosen.is_dir(), "created");
    assert!(settings.inbox_set);
    assert_eq!(settings.inbox, Some(dunce::canonicalize(&chosen).unwrap()));

    assert!(!engine.set_drops_receive(false).unwrap().receive);
    assert!(engine.set_drops_receive(true).unwrap().receive);

    let settings = engine.set_drop_inbox(None).unwrap();
    assert!(!settings.inbox_set);
    assert_eq!(settings.inbox, relay_engine::default_drop_inbox());
}

#[test]
fn the_inbox_stays_out_of_the_relay_home() {
    let home = TempDir::new().unwrap();
    let mut engine = Engine::init(home.path(), "desk").unwrap();
    let inside = home.path().join("inbox");
    assert!(matches!(
        engine.set_drop_inbox(Some(&inside)),
        Err(EngineError::OverlapsRelayHome)
    ));
    let file = TempDir::new().unwrap();
    let not_dir = file.path().join("f");
    std::fs::write(&not_dir, b"x").unwrap();
    assert!(engine.set_drop_inbox(Some(&not_dir)).is_err());
}

#[test]
fn a_server_takes_no_drops_unless_asked() {
    let home = TempDir::new().unwrap();
    let data = TempDir::new().unwrap();
    let mut engine = Engine::init(home.path(), "nas").unwrap();
    engine.set_server_data(data.path()).unwrap();
    assert!(!engine.drop_settings().unwrap().receive);
    assert!(engine.set_drops_receive(true).unwrap().receive);
}
