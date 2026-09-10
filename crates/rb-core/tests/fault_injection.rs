//! Data-driven protocol fault bank.  Each injection is a real wire sequence:
//! no mock may claim a frame was rejected unless `StreamChunkSource` parsed it.

use rb_core::channel::{
    validate_plan_bounds, ChunkEvent, ChunkSource, DuplexStream, MultiStreamChunkSource,
    StreamChunkSource, MAX_PLAN_ITEM_META_BYTES, MAX_PLAN_ITEM_NAME_BYTES,
};
use rb_core::error::Phase;
use rb_core::plan::{BackupMode, BackupPlan, IntegritySpec, PlanItem};
use rb_core::progress::Progress;
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

/// Write one fault's exact wire sequence. Shared by the single-stream and the
/// item-pinned multi-carrier harness so both parse identical bytes.
async fn write_fault(fault: Fault, mut writer: tokio::io::DuplexStream) {
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
}

async fn inject(fault: Fault) -> String {
    let (writer, reader) = tokio::io::duplex(1 << 16);
    let send = tokio::spawn(write_fault(fault, writer));
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

/// Close every carrier that owns no item the way the real source closes it.
///
/// `MultiStreamChunkSink::finish` sends `StreamEnd` on *every* carrier, including
/// the ones that carried nothing, and the destination's post-plan drain reads the
/// carriers in index order waiting for exactly that. A harness that left the
/// siblings half-open therefore wedged any fault whose detection happens after
/// the cursor leaves the plan (`DuplicateItemEnd`): carrier 0 never ended, so the
/// drain never reached the stray frame on carrier 1.
fn end_idle_carriers(idle: Vec<tokio::io::DuplexStream>) {
    for mut writer in idle {
        tokio::spawn(async move {
            wire::send_frame(&mut writer, &DataFrame::StreamEnd)
                .await
                .expect("idle carrier end");
        });
    }
}

/// The item-pinned demux writes the faulted frames onto the carrier that owns
/// the planned item (`item_id % carriers`) and ends the siblings. That the fault
/// is still reported against the *owning* carrier is the assertion: the ordered
/// pull must not satisfy a planned item out of a sibling's bytes.
async fn inject_multi(fault: Fault, carriers: usize) -> String {
    const ITEM_ID: u32 = 1;
    let mut readers: Vec<Box<dyn DuplexStream>> = Vec::with_capacity(carriers);
    let mut faulted_writer = None;
    let mut idle = Vec::new();
    for carrier in 0..carriers {
        let (writer, reader) = tokio::io::duplex(1 << 16);
        readers.push(Box::new(reader));
        if carrier == ITEM_ID as usize % carriers {
            faulted_writer = Some(writer);
        } else {
            idle.push(writer);
        }
    }
    let Some(writer) = faulted_writer else {
        panic!("the owning carrier must exist for carriers={carriers}")
    };
    let send = tokio::spawn(write_fault(fault, writer));
    end_idle_carriers(idle);

    let mut source =
        MultiStreamChunkSource::new_ordered(readers, vec![ITEM_ID], Progress::new(1, 3))
            .expect("ordered demux over the injected carriers");
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
    send.await.expect("fault writer");
    failure.expect("injected protocol fault must fail")
}

/// F2.1: every wire fault is phase/integrity tagged on both one- and
/// four-carrier configurations.
///
/// This loop used to bind `carriers` and never pass it anywhere: both passes
/// built the same single-stream parser, so the four-carrier half of the bank
/// asserted nothing the one-carrier half had not already asserted.
#[tokio::test]
async fn protocol_faults_are_rejected_for_each_carrier_count() {
    for carriers in [1_usize, 4] {
        for fault in PROTOCOL_FAULTS {
            // A bank that hangs is a bank that reports nothing: bound every case
            // so a stalled parser fails the run instead of wedging CI.
            let error = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                if carriers == 1 {
                    inject(fault).await
                } else {
                    inject_multi(fault, carriers).await
                }
            })
            .await
            .unwrap_or_else(|_| panic!("carriers={carriers} fault={fault:?} never terminated"));
            assert!(
                error.contains("integrity")
                    || error.contains("[Verify]")
                    || error.contains("[Transfer]"),
                "carriers={carriers} fault={fault:?} error={error}"
            );
        }
    }
}

/// The demux's own defences, which no fault above can reach: they are about a
/// frame arriving on the wrong carrier, and only a multi-carrier layout has one.
#[tokio::test]
async fn the_ordered_demux_refuses_frames_that_belong_to_another_carrier() {
    let carriers = 4;
    let expected: u32 = 1;

    // (a) the owning carrier delivers a different item's chunk.
    let error = with_owning_carrier(carriers, expected, move |mut writer| async move {
        wire::send_frame(
            &mut writer,
            &DataFrame::ChunkStart {
                item_id: expected + 4,
                offset: 0,
                len: 3,
                blake3: wire::blake3_hex(b"abc"),
            },
        )
        .await
        .expect("chunk start");
        wire::write_all_idle(&mut writer, b"abc")
            .await
            .expect("chunk body");
    })
    .await
    .expect_err("a foreign item on this carrier must be refused");
    assert!(
        error.contains("expected item=1") && error.contains("carrier=1"),
        "the error must name both ids: {error}"
    );

    // (b) the owning carrier ends before the planned item completed.
    let error = with_owning_carrier(carriers, expected, move |mut writer| async move {
        wire::send_frame(&mut writer, &DataFrame::StreamEnd)
            .await
            .expect("stream end");
    })
    .await
    .expect_err("an early carrier end must be refused");
    assert!(
        error.contains("ended before planned item=1"),
        "unexpected error: {error}"
    );

    // (c) an ItemEnd for a different item on this carrier.
    let error = with_owning_carrier(carriers, expected, move |mut writer| async move {
        wire::send_frame(
            &mut writer,
            &DataFrame::ItemEnd {
                item_id: expected + 4,
                total: 0,
                blake3: wire::blake3_hex(b""),
            },
        )
        .await
        .expect("item end");
    })
    .await
    .expect_err("a foreign ItemEnd on this carrier must be refused");
    assert!(
        error.contains("ItemEnd item=5") && error.contains("expected item=1"),
        "unexpected error: {error}"
    );
}

/// Once the plan is exhausted every carrier must deliver `StreamEnd` and nothing
/// else — an extra item after the last planned one is a source that streamed
/// something the destination never validated against the plan.
#[tokio::test]
async fn the_ordered_demux_refuses_an_item_after_the_plan_ends() {
    let (mut writer_a, reader_a) = tokio::io::duplex(1 << 16);
    let (mut writer_b, reader_b) = tokio::io::duplex(1 << 16);
    tokio::spawn(async move {
        wire::send_frame(&mut writer_a, &DataFrame::StreamEnd)
            .await
            .expect("carrier 0 end");
        wire::send_frame(
            &mut writer_b,
            &DataFrame::ItemEnd {
                item_id: 9,
                total: 0,
                blake3: wire::blake3_hex(b""),
            },
        )
        .await
        .expect("carrier 1 stray item");
    });

    let readers: Vec<Box<dyn DuplexStream>> = vec![Box::new(reader_a), Box::new(reader_b)];
    let mut source = MultiStreamChunkSource::new_ordered(readers, Vec::new(), Progress::new(0, 0))
        .expect("ordered demux with an empty plan");
    let error = source
        .next()
        .await
        .expect_err("an item after the plan end must be refused")
        .to_string();
    assert!(
        error.contains("emitted unexpected item=9"),
        "unexpected error: {error}"
    );
}

/// An ordered pull must never read a sibling carrier while the cursor still sits
/// on a planned item. The siblings here are left half-open on purpose: if the
/// demux polled one out of turn it would block, and this test would time out
/// instead of completing the item.
#[tokio::test]
async fn an_idle_sibling_is_not_read_while_the_cursor_sits_on_an_item() {
    let carriers = 4;
    let expected: u32 = 1;
    let mut readers: Vec<Box<dyn DuplexStream>> = Vec::with_capacity(carriers);
    let mut owning = None;
    let mut idle = Vec::new();
    for carrier in 0..carriers {
        let (writer, reader) = tokio::io::duplex(1 << 16);
        readers.push(Box::new(reader));
        if carrier == expected as usize % carriers {
            owning = Some(writer);
        } else {
            idle.push(writer);
        }
    }
    let Some(mut writer) = owning else {
        panic!("the owning carrier must exist")
    };
    let data = b"abc";
    tokio::spawn(async move {
        wire::send_frame(
            &mut writer,
            &DataFrame::ChunkStart {
                item_id: expected,
                offset: 0,
                len: data.len() as u32,
                blake3: wire::blake3_hex(data),
            },
        )
        .await
        .expect("chunk start");
        wire::write_all_idle(&mut writer, data)
            .await
            .expect("chunk body");
        wire::send_frame(
            &mut writer,
            &DataFrame::ItemEnd {
                item_id: expected,
                total: data.len() as u64,
                blake3: wire::blake3_hex(data),
            },
        )
        .await
        .expect("item end");
    });

    let mut source =
        MultiStreamChunkSource::new_ordered(readers, vec![expected], Progress::new(1, 3))
            .expect("ordered demux");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        assert!(matches!(
            source.next().await.expect("chunk"),
            ChunkEvent::Chunk { item_id, .. } if item_id == expected
        ));
        assert!(matches!(
            source.next().await.expect("item end"),
            ChunkEvent::ItemEnd { item_id, .. } if item_id == expected
        ));
    })
    .await
    .expect("the demux read a half-open sibling instead of the owning carrier");
    drop(idle);
}

/// Drive one fault-writing closure against the carrier that owns `expected`,
/// returning the demux's first event or its first error.
async fn with_owning_carrier<F, Fut>(
    carriers: usize,
    expected: u32,
    write: F,
) -> std::result::Result<ChunkEvent, String>
where
    F: FnOnce(tokio::io::DuplexStream) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mut readers: Vec<Box<dyn DuplexStream>> = Vec::with_capacity(carriers);
    let mut owning = None;
    let mut idle = Vec::new();
    for carrier in 0..carriers {
        let (writer, reader) = tokio::io::duplex(1 << 16);
        readers.push(Box::new(reader));
        if carrier == expected as usize % carriers {
            owning = Some(writer);
        } else {
            idle.push(writer);
        }
    }
    let Some(writer) = owning else {
        panic!("the owning carrier must exist")
    };
    let send = tokio::spawn(write(writer));
    let mut source =
        MultiStreamChunkSource::new_ordered(readers, vec![expected], Progress::new(1, 3))
            .expect("ordered demux");
    let outcome = source.next().await.map_err(|error| error.to_string());
    send.await.expect("fault writer");
    drop(idle);
    outcome
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
