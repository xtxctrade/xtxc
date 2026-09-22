use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[test]
#[ignore = "requires AWS same-bank resource-admission SBF artifact"]
fn full_curve_oom_and_pruned_fallback_share_one_execution_bank_and_floor() {
    let report = PathBuf::from(
        std::env::var("SKEW_RESOURCE_ADMISSION_REPORT")
            .expect("explicit AWS resource-admission report path"),
    );
    let value: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(value["schema"], "skew.stockmesh.resource-admission-sbf/v1");
    assert_eq!(value["fullResult"], "REJECTED");
    assert!(value["fullProgramResult"]
        .as_str()
        .unwrap()
        .contains("ProgramFailedToComplete"));
    assert_eq!(value["fallbackResult"], "ADMITTED");
    assert_eq!(value["fallbackVenue"], "ByrealClmm");
    assert_eq!(value["submitted"], false);
    assert_eq!(value["fullCandidateCount"], 3);
    assert_eq!(value["fallbackCandidateCount"], 2);
    let full_cu = value["fullComputeUnits"].as_u64().unwrap();
    let fallback_cu = value["fallbackComputeUnits"].as_u64().unwrap();
    assert!(fallback_cu > 0 && fallback_cu < full_cu && full_cu < 1_400_000);
    assert!(
        value["sameEconomicExposureFloorQ32"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );
    assert!(
        value["fallbackOutputSpyonAtoms"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > 0
    );

    let bank_bytes = std::fs::read(report.with_file_name("execution-bank.json")).unwrap();
    assert_eq!(
        format!("{:x}", Sha256::digest(&bank_bytes)),
        value["executionBankSha256"].as_str().unwrap()
    );
    let bank: Value = serde_json::from_slice(&bank_bytes).unwrap();
    assert_eq!(bank["bank"]["context"]["slot"], value["stateSlot"]);
}
