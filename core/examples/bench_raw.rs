//! CPUだけの実行速度。`Machine::step()` のラッパー分を外し、
//! `cpu::step()` だけを回して命令毎秒を出す。
//!
//! **`bench` との差が「CPUが機械になった分の値段」である。**割り込みの確認、
//! 装置のカウントダウン、HLT判定、BIOS入口の判定、トラップフラグの確認 —
//! ELKSの起動に必要だったものが、そのまま1命令あたりのコストとして出る。
//!
//! Tier 3以降で `step()` に何か足したら、この差が広がっていないかを見ること。
//! 差が開いていれば、それはホットパスに載せてはいけないものを載せた合図である。
//!
//! ```text
//! cargo run --release --example bench_raw -- asm/bench.bin
//! ```

use rustx86_core::{cpu, Machine};
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "asm/bench.bin".into());
    let sector = std::fs::read(&path).expect("bench.bin");

    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");

    let t0 = Instant::now();
    let mut n = 0u64;
    while !m.halted && n < 20_000_000_000 {
        cpu::step(&mut m);
        n += 1;
    }
    let el = t0.elapsed().as_secs_f64();
    println!("{n} 命令 / {el:.2}秒 = {:.1} MIPS", n as f64 / el / 1e6);
    println!("1命令あたり {:.2} ns", el * 1e9 / n as f64);
}
