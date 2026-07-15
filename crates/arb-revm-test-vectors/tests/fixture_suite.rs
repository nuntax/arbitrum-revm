use std::{fs, path::Path};

use arb_stf_fixture::{FixtureCase, FixtureManifest, ObjectStore};

#[test]
fn committed_fixture_suite_is_self_consistent() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/stf/v2");
    let manifest: FixtureManifest =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    manifest.validate().unwrap();

    let store = ObjectStore::new(&root);
    for entry in manifest.cases {
        let case: FixtureCase =
            serde_json::from_slice(&fs::read(root.join(&entry.path)).unwrap()).unwrap();
        case.validate().unwrap();
        assert_eq!(case.id, entry.id);
        assert_eq!(case.labels, entry.labels);
        assert_eq!(case.prestate_object().sha256, entry.prestate_sha256);
        assert_eq!(case.input_object().sha256, entry.input_sha256);
        assert_eq!(case.expected.object.sha256, entry.expected_sha256);
        store.verify(case.prestate_object()).unwrap();
        store.verify(case.input_object()).unwrap();
        store.verify(&case.expected.object).unwrap();
    }
}
