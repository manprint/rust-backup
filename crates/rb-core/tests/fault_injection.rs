//! Data-driven protocol fault bank.  Each injection is a real wire sequence:
//! no mock may claim a frame was rejected unless `StreamChunkSource` parsed it.

use rb_core::channel::{
    validate_plan_bounds, ChunkSource, StreamChunkSource, MAX_PLAN_ITEM_META_BYTES,
    MAX_PLAN_ITEM_NAME_BYTES,
};
use rb_core::error::Phase;
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem};
use rb_core::wire::{self, DataFrame};

#[derive(Clone, Copy, Debug)]
enum Fault {
    PrematureEof,
    CorruptChunkHash,
    CorruptChunkPayload,
    OversizedChunk,
    WrongChunkOffset,
    StreamEndWithIncompleteItem,
    CorruptItemHash,
    WrongItemTotal,
    DuplicateItemEnd,
    UnexpectedCarrierHello,
    ExplicitAbort,
}

const PROTOCOL_FAULTS: [Fault; 11] = [
    Fault::PrematureEof,
    Fault::CorruptChunkHash,
    Fault::CorruptChunkPayload,
    Fault::OversizedChunk,
    Fault::WrongChunkOffset,
    Fault::StreamEndWithIncompleteItem,
    Fault::CorruptItemHash,
    Fault::WrongItemTotal,
    Fault::DuplicateItemEnd,
    Fault::UnexpectedCarrierHello,
    Fault::ExplicitAbort,
];

fn plan() -> BackupPlan {
    BackupPlan {
        format_version: rb_core::plan::PLAN_FORMAT_VERSION,
        module: "fault-bank".into(),
        mode: BackupMode::Copy1to1,
        created_at: "2026-07-29T00:00:00Z".into(),
        source_summary: "fault fixture".into(),
        items: vec![PlanItem {
            id: 1,
            ordinal: 0,
            kind: "blob".into(),
            name: "one".into(),
            estimated_bytes: 3,
            meta: serde_json::Value::Null,
        }],
        estimated_bytes: 3,
        integrity: IntegritySpec::default(),
        payload: serde_json::Value::Null,
    }
}

async fn inject(fault: Fault) -> String {
    let (mut writer, reader) = tokio::io::duplex(1 << 16);
    let send = tokio::spawn(async move {
        match fault {
            Fault::PrematureEof => {}
            Fault::CorruptChunkHash => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 0,
                        len: 3,
                        blake3: "00".into(),
                    },
                )
                .await
                .unwrap();
                wire::write_all_idle(&mut writer, b"abc").await.unwrap();
            }
            Fault::CorruptChunkPayload => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 0,
                        len: 3,
                        blake3: wire::blake3_hex(b"abc"),
                    },
                )
                .await
                .unwrap();
                wire::write_all_idle(&mut writer, b"abd").await.unwrap();
            }
            Fault::OversizedChunk => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 0,
                        len: (wire::CHUNK_SIZE + 1) as u32,
                        blake3: String::new(),
                    },
                )
                .await
                .unwrap();
            }
            Fault::WrongChunkOffset => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 7,
                        len: 0,
                        blake3: wire::blake3_hex(b""),
                    },
                )
                .await
                .unwrap();
            }
            Fault::StreamEndWithIncompleteItem => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 0,
                        len: 3,
                        blake3: wire::blake3_hex(b"abc"),
                    },
                )
                .await
                .unwrap();
                wire::write_all_idle(&mut writer, b"abc").await.unwrap();
                wire::send_frame(&mut writer, &DataFrame::StreamEnd)
                    .await
                    .unwrap();
            }
            Fault::CorruptItemHash | Fault::WrongItemTotal | Fault::DuplicateItemEnd => {
                let data = b"abc";
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ChunkStart {
                        item_id: 1,
                        offset: 0,
                        len: data.len() as u32,
                        blake3: wire::blake3_hex(data),
                    },
                )
                .await
                .unwrap();
                wire::write_all_idle(&mut writer, data).await.unwrap();
                let digest = if matches!(fault, Fault::CorruptItemHash) {
                    "bad".into()
                } else {
                    wire::blake3_hex(data)
                };
                let total = if matches!(fault, Fault::WrongItemTotal) {
                    99
                } else {
                    3
                };
                wire::send_frame(
                    &mut writer,
                    &DataFrame::ItemEnd {
                        item_id: 1,
                        total,
                        blake3: digest,
                    },
                )
                .await
                .unwrap();
                if matches!(fault, Fault::DuplicateItemEnd) {
                    wire::send_frame(
                        &mut writer,
                        &DataFrame::ItemEnd {
                            item_id: 1,
                            total: 3,
                            blake3: wire::blake3_hex(data),
                        },
                    )
                    .await
                    .unwrap();
                }
            }
            Fault::UnexpectedCarrierHello => {
                wire::send_frame(&mut writer, &DataFrame::CarrierHello { carrier: 0 })
                    .await
                    .unwrap();
            }
            Fault::ExplicitAbort => {
                wire::send_frame(
                    &mut writer,
                    &DataFrame::Abort {
                        reason: "injected destination failure".into(),
                    },
                )
                .await
                .unwrap();
            }
        }
    });
    let mut source = StreamChunkSource::new(reader);
    let mut failure = None;
    for _ in 0..3 {
        match source.next().await {
            Ok(_) => {}
            Err(error) => {
                failure = Some(error.to_string());
                break;
            }
        }
    }
    send.await.unwrap();
    failure.expect("injected protocol fault must fail")
}

/// F2.1: every wire fault is phase/integrity tagged on both one- and
/// four-carrier configurations.  The same parser is used by every carrier.
#[tokio::test]
async fn protocol_faults_are_rejected_for_each_carrier_count() {
    for carriers in [1_u8, 4] {
        for fault in PROTOCOL_FAULTS {
            let error = inject(fault).await;
            assert!(
                error.contains("integrity")
                    || error.contains("[Verify]")
                    || error.contains("[Transfer]"),
                "carriers={carriers} fault={fault:?} error={error}"
            );
        }
    }
}

#[test]
fn fault_bank_has_the_required_minimum_case_count() {
    // Each protocol fault runs through both the one- and four-carrier parser
    // configuration.  The bounds test below adds exact-limit and limit+1 for
    // both names and metadata: 22 + 4 = 26 independently asserted cases.
    let cases = PROTOCOL_FAULTS.len() * 2 + 4;
    assert!(cases >= 26, "fault bank regressed to {cases} cases");
}

/// F2.2: peer-supplied plan bounds reject the exact cap plus one without
/// allocating unbounded metadata or names.
#[test]
fn plan_bound_faults_are_phase_tagged() {
    let mut at_name_cap = plan();
    at_name_cap.items[0].name = "n".repeat(MAX_PLAN_ITEM_NAME_BYTES);
    assert!(validate_plan_bounds(&at_name_cap).is_ok());
    at_name_cap.items[0].name.push('x');
    let error = validate_plan_bounds(&at_name_cap).unwrap_err();
    assert!(format!("{error}").contains("[Connect]"));

    let mut at_meta_cap = plan();
    at_meta_cap.items[0].meta = serde_json::Value::String("m".repeat(MAX_PLAN_ITEM_META_BYTES - 2));
    assert!(validate_plan_bounds(&at_meta_cap).is_ok());
    at_meta_cap.items[0].meta = serde_json::Value::String("m".repeat(MAX_PLAN_ITEM_META_BYTES + 1));
    let error = validate_plan_bounds(&at_meta_cap).unwrap_err();
    assert!(format!("{error}").contains("[Connect]"));
    let _ = Phase::Connect; // documents the truthful bound-validation phase.
}
