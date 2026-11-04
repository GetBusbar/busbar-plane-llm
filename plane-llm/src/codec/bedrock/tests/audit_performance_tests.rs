use super::*;

#[test]
fn marker_merge_matches_insert_for_duplicates_and_out_of_range_positions() {
    // Enumerate unsorted inputs, repeated indices, empty content and append positions.
    for content_len in 0..=4 {
        for mut code in 0..3125 {
            let mut entries = Vec::new();
            for marker in 0..5 {
                let index = [0, 1, 2, 5, 100][code % 5];
                code /= 5;
                entries.push(serde_json::json!({"i": index, "block": {"marker": marker}}));
            }
            entries.push(serde_json::json!({"i": -1, "block": "invalid"}));
            entries.push(serde_json::json!({"i": 0}));
            let original: Vec<_> = (0..content_len).map(|i| serde_json::json!(i)).collect();
            let mut expected = original.clone();
            let mut pending: Vec<_> = entries
                .iter()
                .filter_map(|entry| {
                    Some((
                        entry.get("i")?.as_u64()? as usize,
                        entry.get("block")?.clone(),
                    ))
                })
                .collect();
            pending.sort_by_key(|(index, _)| *index);
            for (index, block) in pending {
                expected.insert(index.min(expected.len()), block);
            }
            let mut actual = original;
            splice_cache_points(&mut actual, &entries);
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn document_bytes_precede_text_and_content_joining_is_preserved() {
    use crate::codec::ir::{IrBlock, IrImageSource, IrMediaKind};
    for (source, expected) in [
        (
            serde_json::json!({"bytes":"Ynl0ZXM=", "text":"ignored"}),
            "Ynl0ZXM=",
        ),
        (
            serde_json::json!({"text":"hello", "content":[{"text":"ignored"}]}),
            "aGVsbG8=",
        ),
        (
            serde_json::json!({"content":[{"text":"a"}, {}, {"text":"b"}]}),
            "YQpi",
        ),
    ] {
        let block = bedrock_media_block(
            IrMediaKind::Document,
            &serde_json::json!({"format":"txt", "source":source}),
        );
        let IrBlock::Media {
            source: IrImageSource::Base64 { data, .. },
            ..
        } = block
        else {
            panic!("expected inline document");
        };
        assert_eq!(data, expected);
    }
}
