//! 実行速度の計測。ブートセクタをHLTまで走らせて命令毎秒を出す。
//!
//! ワークロードは `asm/bench.asm` (コミット済み)。命令数が固定なので、
//! 装置や割り込みを足した後に同じものを流せば**劣化がそのまま見える**。
//!
//! ```text
//! nasm -f bin -o asm/bench.bin asm/bench.asm   # 変更時のみ
//! cargo run --release --example bench -- asm/bench.bin
//! ```
//!
//! `--release` は必須。デバッグビルドでは1桁遅くなり、比較の意味がなくなる。

use rustx86_core::Machine;
use std::time::Instant;

/// 打ち切り上限。ワークロードがHLTで終わらなかったことを検出するための番人で、
/// 通常の測定でここに到達することはない
const MAX_INSTRUCTIONS: u64 = 20_000_000_000;

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "asm/bench.bin".into());
    let sector = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("{path} を読めない ({e})。nasm -f bin -o asm/bench.bin asm/bench.asm")
    });

    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");

    let t0 = Instant::now();
    let n = m.run(MAX_INSTRUCTIONS);
    let el = t0.elapsed().as_secs_f64();

    // HLTに到達していない = 上限で打ち切られた測定。命令数が実行時間に
    // 依存してしまうので、MIPSの比較に使ってはいけない
    if !m.halted {
        eprintln!("警告: HLTに到達せず上限で打ち切った。この値は比較に使えない");
    }

    println!("{n} 命令 / {el:.2}秒 = {:.1} MIPS", n as f64 / el / 1e6);
    println!("1命令あたり {:.2} ns", el * 1e9 / n as f64);

    if cfg!(debug_assertions) {
        eprintln!("注意: デバッグビルドで測定している。--release を付けること");
    }
}
