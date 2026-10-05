//! Server role planning (home server Stage 1).

use relay_core::ConfigChange;
use relay_engine::{Engine, SyncInput, Syncer};
use relay_proto::frame;
use tempfile::TempDir;

struct Setup {
    _dirs: Vec<TempDir>,
    client: Engine,
    server: Engine,
    data: std::path::PathBuf,
}

fn setup(spaces: &[&str]) -> Setup {
    let dirs: Vec<TempDir> = (0..4).map(|_| TempDir::new().unwrap()).collect();
    let mut client = Engine::init(dirs[0].path(), "alpha").unwrap();
    let mut server = Engine::init(dirs[1].path(), "nas").unwrap();
    let server_id = server.device().id;
    client.add_peer("nas", server_id, &[]).unwrap();
    server.add_peer("alpha", client.device().id, &[]).unwrap();
    server.set_peer_manage("alpha", true).unwrap();
    for (i, space) in spaces.iter().enumerate() {
        let mount = dirs[2].path().join(i.to_string());
        std::fs::create_dir_all(&mount).unwrap();
        client.create_space(space).unwrap();
        client.add_mount(space, "files", &mount, &[], &[]).unwrap();
        client.share(space, "nas").unwrap();
    }
    let data = server.set_server_data(dirs[3].path()).unwrap();
    let offers = client.space_offers_for_peer(server_id).unwrap();
    let mut syncer = Syncer::new();
    let peer = client.device().id;
    syncer
        .handle(
            &mut server,
            SyncInput::PeerConnected {
                peer,
                name: "alpha".into(),
            },
            &mut |_| {},
        )
        .unwrap();
    syncer
        .handle(
            &mut server,
            SyncInput::Frame {
                peer,
                body: frame::Body::SpaceOffers(offers),
            },
            &mut |_| {},
        )
        .unwrap();
    Setup {
        _dirs: dirs,
        client,
        server,
        data,
    }
}

#[test]
fn plan_joins_and_attaches_then_is_empty() {
    let mut s = setup(&["Photos"]);
    let plan = s.server.server_plan().unwrap();
    let path = s.data.join("Photos").join("files");
    assert_eq!(
        plan,
        vec![
            ConfigChange::JoinSpace {
                space: "Photos".into(),
                from_peer: s.client.device().id.to_string(),
                wait_ms: 0,
            },
            ConfigChange::AddMount {
                space: "Photos".into(),
                mount: "files".into(),
                path: path.clone(),
                includes: vec![],
                excludes: vec![],
            },
        ]
    );
    for change in &plan {
        s.server.apply_config(change).unwrap();
    }
    assert!(s.server.server_plan().unwrap().is_empty());
    let status = s.server.server_status().unwrap();
    assert_eq!(status.mounts.len(), 1);
    assert_eq!(status.mounts[0].path.as_ref(), Some(&path));
}

#[test]
fn not_a_server_plans_nothing() {
    let mut s = setup(&["Photos"]);
    s.server.clear_server_data().unwrap();
    assert!(s.server.server_plan().unwrap().is_empty());
    assert!(s.server.server_status().unwrap().data.is_none());
}

#[test]
fn revoked_peer_offers_are_ignored() {
    let mut s = setup(&["Photos"]);
    s.server
        .apply_config(&ConfigChange::RevokePeer {
            peer: "alpha".into(),
        })
        .unwrap();
    assert!(s.server.server_plan().unwrap().is_empty());
}

#[test]
fn peer_without_manage_grant_cannot_make_it_join() {
    let mut s = setup(&["Photos"]);
    s.server.set_peer_manage("alpha", false).unwrap();
    assert!(s.server.server_plan().unwrap().is_empty());
}

#[test]
fn unsafe_space_names_are_skipped() {
    let mut s = setup(&["..", "Docs"]);
    let plan = s.server.server_plan().unwrap();
    assert!(plan.iter().all(|c| c.space() == Some("Docs")), "{plan:?}");
    assert!(!s.data.parent().unwrap().join("files").exists());
    for change in &plan {
        s.server.apply_config(change).unwrap();
    }
}

#[test]
fn data_folder_may_not_overlap_relay_home() {
    let dir = TempDir::new().unwrap();
    let mut engine = Engine::init(dir.path(), "nas").unwrap();
    assert!(engine.set_server_data(&dir.path().join("data")).is_err());
    assert!(engine.set_server_data(dir.path()).is_err());
}
