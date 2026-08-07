//! ブートセクタを実行してコンソール出力を表示する
use rustx86_core::Machine;

fn main() {
    let path = std::env::args().nth(1).expect("usage: run <boot.bin>");
    let sector = std::fs::read(&path).expect("read boot sector");
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    let n = m.run(1_000_000);
    println!("{}", m.console_string());
    eprintln!("[{} instructions, halted={}]", n, m.halted);
}
