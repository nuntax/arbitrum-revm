use std::{fs, path::Path};

use arb_revm_test_vectors::run_case;
use arb_stf_fixture::{FixtureCase, FixtureInput, FixtureManifest, FixturePrestate};

#[test]
fn complete_derived_transaction_cases_match_nitro_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/stf/v2");
    let manifest: FixtureManifest =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    manifest.validate().unwrap();

    for entry in manifest.cases {
        let case: FixtureCase =
            serde_json::from_slice(&fs::read(root.join(entry.path)).unwrap()).unwrap();
        if matches!(case.prestate, FixturePrestate::Complete { .. })
            && matches!(case.input, FixtureInput::DerivedTransactions { .. })
        {
            let report = run_case(&root, &case);
            assert!(
                report.is_parity(),
                "fixture {} mismatched Nitro output:\n{}",
                case.id,
                report.mismatches.join("\n")
            );
        }
    }
}
