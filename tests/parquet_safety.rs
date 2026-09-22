//! Compact metadata bounds are checked through the actual Parquet reader.

use std::sync::Arc;

use datafusion::arrow::array::UInt64Array;
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::file::reader::SerializedFileReader;

fn archive_with_metadata_change(change: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![Field::new(
        "timestamp",
        DataType::UInt64,
        false,
    )]));
    let batch =
        RecordBatch::try_new(schema.clone(), vec![Arc::new(UInt64Array::from(vec![42]))]).unwrap();
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    assert!(accepts(&bytes));
    let footer = bytes.len() - 8;
    let metadata_len = u32::from_le_bytes(bytes[footer..footer + 4].try_into().unwrap()) as usize;
    let metadata_start = footer - metadata_len;
    let mut metadata = bytes[metadata_start..footer].to_vec();
    change(&mut metadata);
    bytes.truncate(metadata_start);
    bytes.extend_from_slice(&metadata);
    bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"PAR1");
    bytes
}

fn accepts(bytes: &[u8]) -> bool {
    let archive = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(archive.path(), bytes).unwrap();
    SerializedFileReader::new(archive.reopen().unwrap()).is_ok()
}

// Parquet's decoder reads varints leniently: continuation bytes past the
// integer's width wrap instead of failing, and `i32` fields truncate. Either
// way the loop stops when the input runs out, so the worst outcome is a
// garbled metadata value. These cases pin the property that matters to us:
// opening the file returns, whether it accepts or refuses.
#[test]
fn parquet_metadata_with_an_overlong_integer_does_not_panic() {
    let bytes = archive_with_metadata_change(|metadata| {
        assert_eq!(&metadata[..2], &[0x15, 0x02]);
        metadata.splice(1..1, [0x80; 64]);
    });
    let _ = accepts(&bytes);
}

#[test]
fn parquet_metadata_with_a_32_bit_integer_overflow_does_not_panic() {
    let bytes = archive_with_metadata_change(|metadata| {
        metadata.splice(1..2, [0xff, 0xff, 0xff, 0xff, 0x1f]);
    });
    let _ = accepts(&bytes);
}

fn append_unknown_field(metadata: &mut Vec<u8>, kind: u8, payload: &[u8]) {
    assert_eq!(metadata.pop(), Some(0));
    // Explicit field 32767, outside this schema, exercises the skip path.
    metadata.extend_from_slice(&[kind, 0xfe, 0xff, 0x03]);
    metadata.extend_from_slice(payload);
    metadata.push(0);
}

#[test]
fn parquet_unknown_fields_use_the_same_integer_bounds() {
    let bytes = archive_with_metadata_change(|metadata| {
        append_unknown_field(metadata, 0x06, &[0x80; 11]);
    });
    assert!(!accepts(&bytes));
}

#[test]
fn parquet_unknown_fields_accept_valid_maximum_width_integers() {
    let bytes = archive_with_metadata_change(|metadata| {
        append_unknown_field(
            metadata,
            0x06,
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1],
        );
    });
    assert!(accepts(&bytes));
}

#[test]
fn truncated_double_returns_an_error_instead_of_panicking() {
    let bytes = archive_with_metadata_change(|metadata| {
        append_unknown_field(metadata, 0x07, &[1, 2, 3]);
    });
    assert!(!accepts(&bytes));
}

#[test]
fn impossible_schema_count_is_rejected_before_allocation() {
    let bytes = archive_with_metadata_change(|metadata| {
        assert_eq!(&metadata[..4], &[0x15, 0x02, 0x19, 0x2c]);
        // A list of structs claiming more elements than available bytes.
        metadata.splice(3..4, [0xfc, 0xff, 0xff, 0xff, 0xff, 0x07]);
    });
    assert!(!accepts(&bytes));
}
