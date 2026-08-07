//! 実行速度の計測: ブートセクタをHLTまで走らせて命令毎秒を出す
use rustx86_core::Machine;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench <boot.bin>");
    let sector = std::fs::read(&path).expect("read");
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    let t0 = Instant::now();
    let n = m.run(2_000_000_000);
    let el = t0.elapsed().as_secs_f64();
    println!("{n} 命令 / {el:.2}秒 = {:.1} MIPS", n as f64 / el / 1e6);
}
