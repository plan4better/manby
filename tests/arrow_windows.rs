//! An Arrow table longer than one window of reading, with rows that have no
//! geometry, has its features numbered as tippecanoe numbers the FlatGeobuf
//! file of the same table, with any number of threads and with its chunks in
//! the file of a small memory budget, and its tile-stats keep the smallest
//! sample values of the whole table.

use std::fs::File;
use std::sync::Arc;

use arrow_array::{BinaryArray, Int64Array, RecordBatch, StringArray};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema};
use manby::options::{parse_args, set_tippecanoe_globals};

/// About three windows of 2^18 rows.
const ROWS: usize = 700_000;
const CPUS: usize = 4;

/// Rows without a geometry are skipped, so that ranges read fewer features
/// than they have rows.
fn has_geometry(row: usize) -> bool {
    row % 7 != 3 && row % 1000 != 999
}

fn name(row: usize) -> String {
    format!("n{:07}", (row * 7919) % ROWS)
}

fn write_table(path: &std::path::Path) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("row", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("geometry", DataType::Binary, true),
    ]));
    let mut w = StreamWriter::try_new(File::create(path).unwrap(), &schema).unwrap();
    // batches that do not line up with tippecanoe's batches or the windows
    let mut start = 0;
    while start < ROWS {
        let end = (start + 9973).min(ROWS);
        let rows: Vec<usize> = (start..end).collect();
        let wkb = |r: usize| {
            let mut b = vec![1u8];
            b.extend(1u32.to_le_bytes());
            b.extend(((r % 1000) as f64 * 0.01).to_le_bytes());
            b.extend(((r / 1000) as f64 * 0.01).to_le_bytes());
            b
        };
        let geometry: Vec<Option<Vec<u8>>> = rows
            .iter()
            .map(|&r| has_geometry(r).then(|| wkb(r)))
            .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(rows.iter().map(|&r| r as i64))),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|&r| name(r)))),
                Arc::new(BinaryArray::from_iter(
                    geometry.iter().map(|g| g.as_deref()),
                )),
            ],
        )
        .unwrap();
        w.write(&batch).unwrap();
        start = end;
    }
    w.finish().unwrap();
}

/// tippecanoe's `seq` of each row that has a feature: the rows go in batches
/// of `CPUS * 500 + 1`, each parsed in `CPUS` ranges, each range numbered from
/// its offset in its batch, and the next batch from the end of the batch's
/// last range.
fn expected() -> Vec<Option<u64>> {
    let mut seq = vec![None; ROWS];
    let batch = CPUS * 500 + 1;
    let mut base = 0u64;
    let mut start = 0;
    while start < ROWS {
        let size = batch.min(ROWS - start);
        let mut last_end = base;
        for i in 0..CPUS {
            let (a, b) = (start + size * i / CPUS, start + size * (i + 1) / CPUS);
            let range_base = base + (a - start) as u64;
            let mut n = 0;
            for (row, s) in seq.iter_mut().enumerate().take(b).skip(a) {
                if has_geometry(row) {
                    *s = Some(range_base + n);
                    n += 1;
                }
            }
            last_end = range_base + n;
        }
        base = last_end;
        start += size;
    }
    seq
}

#[test]
fn numbered_as_tippecanoe_across_windows() {
    let dir = std::env::temp_dir().join(format!("manby-arrow-windows-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("table.arrows");
    write_table(&input);
    let want = expected();
    let mut names: Vec<String> = (0..ROWS).filter(|&r| has_geometry(r)).map(name).collect();
    names.sort();
    names.truncate(1000);

    // the budget is the process's, so the run with one comes last
    for (threads, budget) in [(1, None), (3, None), (8, Some(4u64 << 20))] {
        if let Some(b) = budget {
            manby::buf::set_budget(b, &dir);
        }
        let argv: Vec<String> = ["tippecanoe", "-q", "-f", "-z5", "-l", "layer", "-o"]
            .iter()
            .map(|s| s.to_string())
            .chain([
                dir.join("out.pmtiles").to_string_lossy().into_owned(),
                input.to_string_lossy().into_owned(),
            ])
            .collect();
        let mut o = parse_args(&argv).unwrap();
        o.tippecanoe_cpus = CPUS;
        o.threads = threads;
        o.resolve().unwrap();
        set_tippecanoe_globals(&o, o.cluster_maxzoom);
        let store = manby::ingest::read_inputs(&o).unwrap();
        assert_eq!(manby::buf::spilling(), budget.is_some());

        let row_key = store.keys.iter().position(|k| k == "row").unwrap() as u32;
        assert_eq!(
            store.features.len(),
            want.iter().filter(|s| s.is_some()).count()
        );
        for f in store.features.iter() {
            let a = store
                .feature_attrs(f)
                .iter()
                .find(|a| a.key == row_key)
                .unwrap();
            let row: usize = store.values.get(a.value).s.parse().unwrap();
            assert_eq!(
                Some(f.seq),
                want[row],
                "row {} with {} threads",
                row,
                threads
            );
        }

        let stats = &store.layermap()["layer"].tilestats["name"];
        let samples: Vec<&str> = stats.sample_values.iter().map(|v| v.s.as_str()).collect();
        assert_eq!(samples, names, "name samples with {} threads", threads);
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
