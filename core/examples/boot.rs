//! ディスクイメージからOSを起動する。
//!
//! ブラウザより先にCLIを作っているのは、**ここが「直す」作業だから**である。
//! どこで止まったかを素早く繰り返し見たいので、grepもdiffも効くCLIが速い。
//! UARTは `feed()` / `tx` というバイト列の口を持っているので、
//! シェルが出た後にブラウザへ繋いでもコアの作り直しは起きない。
//!
//! ```text
//! cargo run --release --example boot -- images/fd1440.img
//! ```

use rustx86_core::Machine;

fn main() {
    let path = std::env::args().nth(1).expect("usage: boot <disk.img>");
    let max: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000_000);

    let image = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut n = 0u64;
        while n < max {
            // HLT中も装置は進み続ける。タイマ割り込みで目を覚ますので、
            // 「保留が無い」だけで打ち切ってはいけない
            if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
                break;
            }
            m.step();
            n += 1;
        }
        n
    }));

    // 止まった理由より先に、**ゲストが何を出したか**を見せる。
    // 大抵はそこに手がかりが書いてある
    let uart = m.devices.uart.tx_string();
    if !uart.is_empty() {
        println!("--- シリアル (COM1) ---\n{uart}");
    }
    let con = m.console_string();
    if !con.is_empty() {
        println!("--- BIOS画面 (INT 10h) ---\n{con}");
    }
    let screen = m.text_screen_string();
    if !screen.is_empty() {
        println!("--- テキストVRAM ---\n{screen}\n--- (ここまで) ---");
    }

    println!("--- 状態 ---");
    match result {
        Ok(n) => println!("{n} 命令 実行。halted={}", m.halted),
        Err(_) => println!("panic で停止 (上のメッセージ参照)"),
    }
    println!(
        "CS:IP={:04x}:{:04x} SP={:04x} flags={:04x}",
        m.cpu.sregs[rustx86_core::cpu::CS],
        m.cpu.ip,
        m.cpu.regs[rustx86_core::cpu::SP] as u16,
        m.cpu.flags
    );

    // ベクタごとの回数と初出。実OSのデバッグはここから始まる
    let fired: Vec<String> = (0..256)
        .filter(|v| m.int_counts[*v] > 0)
        .map(|v| {
            let (cs, ip) = m.int_first[v];
            format!("{v:#04x}×{} ({cs:04x}:{ip:04x})", m.int_counts[v])
        })
        .collect();
    if !fired.is_empty() {
        println!("割り込み (ベクタ×回数、初出): {}", fired.join("  "));
    }
    if !m.int_recent.is_empty() {
        let s: Vec<String> = m
            .int_recent
            .iter()
            .map(|(v, cs, ip)| format!("{v:#04x}@{cs:04x}:{ip:04x}"))
            .collect();
        println!("直近の割り込み: {}", s.join(" → "));
    }

    // 初出位置の命令バイト。「本物の例外」か「ゴミを実行した」かはここで分かる
    for v in [0u8, 1, 3, 4, 6] {
        if m.int_counts[v as usize] > 0 {
            let (cs, ip) = m.int_first[v as usize];
            let a = rustx86_core::cpu::operand::linear(cs, ip);
            let before: Vec<String> =
                (1..=6).rev().map(|i| format!("{:02x}", m.read8(a - i))).collect();
            let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
            println!(
                "INT {v:#04x} 初出 {cs:04x}:{ip:04x}  直前[{}]  IP以降[{}]",
                before.join(" "),
                b.join(" ")
            );
        }
    }

    if let Some((vec, cs, ip)) = m.first_fault {
        let a = rustx86_core::cpu::operand::linear(cs, ip);
        let b: Vec<String> = (0..8).map(|i| format!("{:02x}", m.read8(a + i))).collect();
        println!(
            "最初のCPU例外: INT {vec:#04x} @ {cs:04x}:{ip:04x}  命令バイト: {}",
            b.join(" ")
        );
    }

    // 止まった/回り続けている位置の周辺を見せる。
    // 逆アセンブラは持っていないので生バイトだが、オペコード表と突き合わせれば読める
    let base = rustx86_core::cpu::operand::linear(m.cpu.sregs[rustx86_core::cpu::CS], m.cpu.ip);
    let dump: Vec<String> = (0..16)
        .map(|i| format!("{:02x}", m.read8(base.wrapping_add(i))))
        .collect();
    println!("CS:IP の命令バイト: {}", dump.join(" "));
    let back: Vec<String> = (1..=8)
        .rev()
        .map(|i| format!("{:02x}", m.read8(base.wrapping_sub(i))))
        .collect();
    println!("直前8バイト        : {}", back.join(" "));

    // 未接続ポートは「ゲストが何を探そうとしたか」の記録になる。
    // 装置を足す順番を決める手がかり
    if !m.unhandled_io.is_empty() {
        let list: Vec<String> = m
            .unhandled_io
            .iter()
            .take(24)
            .map(|p| format!("{p:#06x}"))
            .collect();
        println!(
            "触られた未接続ポート ({}件): {}",
            m.unhandled_io.len(),
            list.join(" ")
        );
    }
}
