//! Command-line tools for measuring the data layer without a window.
//!
//!   rowscope --bench gen OUT.csv ROWS     write a synthetic CSV
//!   rowscope --bench FILE [SEARCH]        time load, search, filter, sort

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::data::query::{self, ColFilter, Cond, Edits, NumOp, Query, SortSpec, TextOp};
use crate::data::{self, LoadEvent};

pub fn run(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("gen") => {
            let out = &args[1];
            let rows: u64 = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(1_000_000);
            generate(Path::new(out), rows).expect("generate");
        }
        Some(file) => bench(Path::new(file), args.get(1).map(String::as_str)),
        None => eprintln!("usage: rowscope --bench gen OUT ROWS | --bench FILE [SEARCH]"),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn generate(path: &Path, rows: u64) -> std::io::Result<()> {
    const FIRST: &[&str] = &[
        "Ava", "Liam", "Noah", "Emma", "Olivia", "Mia", "Lucas", "Sofia", "Ethan", "Zoe", "Mateo",
        "Isla", "Kai", "Nora", "Leo", "Aria", "Hugo", "Iris", "Omar", "Yuki",
    ];
    const LAST: &[&str] = &[
        "Smith", "Garcia", "Chen", "Patel", "Müller", "Rossi", "Kim", "Nguyen", "Silva", "Novak",
        "Haddad", "Okafor", "Larsen", "Dubois", "Tanaka", "O'Brien",
    ];
    const CITY: &[(&str, &str)] = &[
        ("New York", "United States"),
        ("Boston", "United States"),
        ("Austin", "United States"),
        ("Seattle", "United States"),
        ("Toronto", "Canada"),
        ("London", "United Kingdom"),
        ("Paris", "France"),
        ("Berlin", "Germany"),
        ("Madrid", "Spain"),
        ("Rome", "Italy"),
        ("Tokyo", "Japan"),
        ("Seoul", "South Korea"),
        ("Sydney", "Australia"),
        ("São Paulo", "Brazil"),
        ("Mumbai", "India"),
        ("Lagos", "Nigeria"),
    ];
    const CAT: &[&str] = &[
        "Hardware", "Software", "Services", "Books", "Garden", "Toys", "Grocery", "Sports",
    ];
    const STATUS: &[&str] = &["Shipped", "Pending", "Delivered", "Returned", "Cancelled"];
    let t0 = Instant::now();
    let mut w = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(path)?);
    writeln!(
        w,
        "order_id,date,customer,city,country,category,status,quantity,unit_price,total,notes"
    )?;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for i in 0..rows {
        let (city, country) = CITY[rng.below(CITY.len() as u64) as usize];
        let q = 1 + rng.below(20);
        let price = (rng.below(100_000) as f64) / 100.0 + 0.99;
        let day = rng.below(365 * 4);
        let (y, rest) = (2021 + day / 365, day % 365);
        let (m, d) = (1 + rest / 31 % 12, 1 + rest % 28);
        let notes = match rng.below(10) {
            0 => "\"Gift wrap, please\"",
            1 => "\"Leave at door; ring \"\"twice\"\"\"",
            2 => "Expedite",
            _ => "",
        };
        writeln!(
            w,
            "{},{y}-{m:02}-{d:02},{} {},{city},{country},{},{},{q},{price:.2},{:.2},{notes}",
            100_000 + i,
            FIRST[rng.below(FIRST.len() as u64) as usize],
            LAST[rng.below(LAST.len() as u64) as usize],
            CAT[rng.below(CAT.len() as u64) as usize],
            STATUS[rng.below(STATUS.len() as u64) as usize],
            price * q as f64,
        )?;
    }
    w.flush()?;
    let size = std::fs::metadata(path)?.len();
    eprintln!(
        "wrote {} rows, {:.1} MB in {:.2?}",
        rows,
        size as f64 / 1e6,
        t0.elapsed()
    );
    Ok(())
}

fn bench(path: &Path, search: Option<&str>) {
    let t0 = Instant::now();
    let rx = data::spawn_load(path.to_path_buf(), None, Default::default());
    let mut rx = rx;
    let mut first_rows = None;
    let data = loop {
        match futures::executor::block_on(futures::StreamExt::next(&mut rx)) {
            Some(LoadEvent::Progress { data, .. }) => {
                if first_rows.is_none() && data.rows > 0 {
                    first_rows = Some((t0.elapsed(), data.rows));
                }
            }
            Some(LoadEvent::Done { data, .. }) => break data,
            Some(LoadEvent::Failed(e)) => {
                eprintln!("failed: {e}");
                return;
            }
            Some(LoadEvent::Sheets(_)) => {}
            None => return,
        }
    };
    let load = t0.elapsed();
    if let Some((t, n)) = first_rows {
        eprintln!("first {n} rows visible after {t:.2?}");
    }
    eprintln!(
        "loaded {} rows in {load:.2?} ({:.1} MB index)",
        data.rows,
        data.heap_bytes() as f64 / 1e6
    );
    let cancel = AtomicBool::new(false);
    let edits = Edits::default();
    let ncols = data.row_fields(0, 1000).len();
    let time = |label: &str, q: Query| {
        let t = Instant::now();
        let r = query::run(&data, 1, &q, &edits, ncols, &cancel)
            .unwrap()
            .unwrap();
        eprintln!(
            "{label:<34} {:>10} rows  {:>9.2?}",
            r.rows.len(),
            t.elapsed()
        );
    };
    time(
        "search",
        Query {
            search: search.unwrap_or("tokyo").into(),
            ..Default::default()
        },
    );
    time(
        "search (rare)",
        Query {
            search: "Haddad".into(),
            ..Default::default()
        },
    );
    time(
        "text filter col 3 = Paris",
        Query {
            filters: vec![ColFilter {
                col: 3,
                cond: Cond::Text {
                    op: TextOp::Equals,
                    needle: "Paris".into(),
                },
            }],
            ..Default::default()
        },
    );
    time(
        "number filter col 9 > 5000",
        Query {
            filters: vec![ColFilter {
                col: 9,
                cond: Cond::Num {
                    op: NumOp::Gt,
                    a: 5000.,
                    b: 0.,
                },
            }],
            ..Default::default()
        },
    );
    time(
        "sort col 9 (number)",
        Query {
            sort: Some(SortSpec {
                col: 9,
                desc: true,
                numeric: true,
            }),
            ..Default::default()
        },
    );
    time(
        "sort col 2 (text)",
        Query {
            sort: Some(SortSpec {
                col: 2,
                desc: false,
                numeric: false,
            }),
            ..Default::default()
        },
    );
    let t = Instant::now();
    let d = query::distinct(&data, 1, None, 3, false, &edits, &cancel).unwrap();
    eprintln!(
        "{:<34} {:>10} vals  {:>9.2?}",
        "distinct col 3",
        d.values.len(),
        t.elapsed()
    );
    // Export every row (the raw-copy path) and a search result.
    let out = path.with_extension("export-test.csv");
    let progress = std::sync::atomic::AtomicU64::new(0);
    let t = Instant::now();
    let n = query::export_csv(
        &out,
        &data,
        1,
        None,
        data.rows - 1,
        &|r| r,
        ncols,
        &edits,
        &progress,
        &cancel,
    )
    .unwrap();
    eprintln!(
        "{:<34} {:>10} rows  {:>9.2?}",
        "export all rows",
        n,
        t.elapsed()
    );
    let same = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "  exported {} bytes (source {})",
        same,
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    );
    let _ = std::fs::remove_file(&out);
    let t = Instant::now();
    let st = query::stats(&data, 1, 0..data.rows - 1, 7..10, &|r| r, &edits, &cancel).unwrap();
    eprintln!(
        "{:<34} {:>10} nums  {:>9.2?}",
        "stats cols 7..10 all rows",
        st.nums,
        t.elapsed()
    );
}
