//! Time `project` on a note of about 32 KiB. `cargo run --release -p katib-editor --example timing`

use std::time::Instant;

fn main() {
    let unit = "## Section\n\nA paragraph with **bold**, *italic*, `code`, a [link](x.md) and [[Page|wiki]] links, long enough to wrap once or twice in a window.\n\n- item one\n- item two with **bold**\n  - nested item\n\n```rust\nfn main() {}\n```\n\n";
    let text = unit.repeat(32 * 1024 / unit.len() + 1);
    let runs = 200;
    let mut times: Vec<f64> = (0..runs)
        .map(|i| {
            let head = (i * 7919) % text.len();
            let head = (0..=head)
                .rev()
                .find(|&h| text.is_char_boundary(h))
                .unwrap();
            let t = Instant::now();
            let p = katib_editor::project(&text, Some(head));
            std::hint::black_box(p);
            t.elapsed().as_secs_f64() * 1000.
        })
        .collect();
    let mut parse: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(katib_core::md::parse(&text));
            t.elapsed().as_secs_f64() * 1000.
        })
        .collect();
    parse.sort_by(f64::total_cmp);
    println!(
        "md::parse alone: median {:.2} ms, p95 {:.2} ms",
        parse[runs / 2],
        parse[runs * 95 / 100]
    );
    times.sort_by(f64::total_cmp);
    println!(
        "project: {} bytes, {runs} runs: median {:.2} ms, p95 {:.2} ms, max {:.2} ms",
        text.len(),
        times[runs / 2],
        times[runs * 95 / 100],
        times[runs - 1]
    );
}
