//! Parser throughput against plain libghostty-vt, fed in PTY-sized chunks.
//!
//!   cargo test --release -p thurm-term --test throughput -- --ignored --nocapture
//!
//! `THURM_BENCH_FILE` picks the input (default: 11 MB of generated prose).

use std::time::{Duration, Instant};

use thurm_proto::PaneSize;
use thurm_term::terminal::{EngineConfig, Terminal};
use thurm_term::vt::Vt;

const COLS: u16 = 79;
const ROWS: u16 = 33;

fn corpus() -> Vec<u8> {
    if let Some(p) = std::env::var_os("THURM_BENCH_FILE") {
        return std::fs::read(p).expect("THURM_BENCH_FILE");
    }
    let words = [
        "the", "quick", "brown", "fox", "jumps", "over", "lazy", "dog", "to", "be", "or", "not",
        "that", "is", "question", "whether", "nobler", "mind", "suffer",
    ];
    let mut out = Vec::with_capacity(11_500_000);
    let mut i = 0usize;
    while out.len() < 11_000_000 {
        let len = 20 + (i * 7) % 60;
        let mut line = String::new();
        while line.len() < len {
            line.push_str(words[(i * 31 + line.len()) % words.len()]);
            line.push(' ');
        }
        out.extend_from_slice(line.trim_end().as_bytes());
        out.push(b'\n');
        i += 1;
    }
    out
}

fn best_of<F: FnMut()>(runs: usize, mut f: F) -> Duration {
    (0..runs)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

#[test]
#[ignore]
fn parser_throughput_full_scrollback() {
    // Steady state: the scrollback is already full, so every new line recycles an old row.
    let data = corpus();
    let size = PaneSize {
        cols: COLS,
        rows: ROWS,
        cell_width: 8,
        cell_height: 16,
    };
    let cfg = EngineConfig {
        scrollback: 10_000,
        ..Default::default()
    };
    let mut ours = Terminal::new(size, cfg);
    ours.advance(&data);
    let thurm = best_of(5, || {
        for c in data.chunks(64 * 1024) {
            ours.advance(c);
        }
    });
    let mut t = Vt::new(COLS, ROWS, 10_000);
    t.write(&data);
    let plain = best_of(5, || {
        for c in data.chunks(64 * 1024) {
            t.write(c);
        }
    });
    println!(
        "full scrollback: libghostty-vt {:.1} ms | thurm {:.1} ms",
        plain.as_secs_f64() * 1e3,
        thurm.as_secs_f64() * 1e3
    );
}

#[test]
#[ignore]
fn parser_throughput() {
    let data = corpus();
    let mb = data.len() as f64 / 1e6;
    for chunk in [1024usize, 16 * 1024] {
        let plain = best_of(5, || {
            let mut t = Vt::new(COLS, ROWS, 10_000);
            for c in data.chunks(chunk) {
                t.write(c);
            }
        });
        let size = PaneSize {
            cols: COLS,
            rows: ROWS,
            cell_width: 8,
            cell_height: 16,
        };
        let cfg = EngineConfig {
            scrollback: 10_000,
            ..Default::default()
        };
        let ours = best_of(5, || {
            let mut t = Terminal::new(size, cfg.clone());
            for c in data.chunks(chunk) {
                t.advance(c);
            }
        });
        let fwd = best_of(5, || {
            let mut t = Terminal::new(size, cfg.clone());
            for c in data.chunks(chunk) {
                std::hint::black_box(t.advance_forward(c));
            }
        });
        let rate = |d: Duration| mb / d.as_secs_f64();
        println!(
            "{:>5} B chunks: libghostty-vt {:>6.1} ms ({:.0} MB/s) | thurm advance {:>6.1} ms ({:.0} MB/s) | advance_forward {:>6.1} ms ({:.0} MB/s)",
            chunk,
            plain.as_secs_f64() * 1e3,
            rate(plain),
            ours.as_secs_f64() * 1e3,
            rate(ours),
            fwd.as_secs_f64() * 1e3,
            rate(fwd),
        );
    }
}
