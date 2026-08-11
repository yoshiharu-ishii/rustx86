//! 起動のどの区間に命令を使っているかを測る (一時的な計測ハーネス)。
//!
//!   cargo run --release --example bootphase
//!
//! 区間: 展開ステブ (最初のシリアル出力まで、画面は無言) → dmesg → シェル

use rustx86_core::{Machine, MachineProfile};

fn main() {
    // 引数でイメージを選べる (既定は bzImage)。vmlinux を渡せば直接ロード経路
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "images/vmlinuz-lts".into());
    let kernel = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let initrd = std::fs::read("images/initramfs-mini").expect("images/initramfs-mini");
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
        .expect("boot");

    let t0 = std::time::Instant::now();
    let mut n: u64 = 0;
    let mut first_serial: Option<(u64, f32)> = None;
    while n < 3_000_000_000 {
        n += m.run(10_000_000);
        if m.trap.is_some() {
            panic!("trap: {:?}", m.trap);
        }
        if first_serial.is_none() && !m.devices.uart.tx.is_empty() {
            first_serial = Some((n, t0.elapsed().as_secs_f32()));
        }
        let s = String::from_utf8_lossy(&m.devices.uart.tx);
        if s.contains("busybox shell") {
            let (fs_n, fs_t) = first_serial.unwrap_or((0, 0.0));
            let total_t = t0.elapsed().as_secs_f32();
            println!("展開ステブ (無言): {}M命令 {:.1}s", fs_n / 1_000_000, fs_t);
            println!(
                "dmesg〜シェル:      {}M命令 {:.1}s",
                (n - fs_n) / 1_000_000,
                total_t - fs_t
            );
            println!("合計:               {}M命令 {:.1}s", n / 1_000_000, total_t);
            // デコードキャッシュの効き具合 (カバレッジ)
            let d = &m.dcache;
            let covered = d.hits + d.fills;
            let seen = covered + d.fallbacks;
            if seen > 0 {
                println!(
                    "dcache: ヒット{}M + 新規{}M = 対象{:.1}% / 従来経路{}M",
                    d.hits / 1_000_000,
                    d.fills / 1_000_000,
                    covered as f64 * 100.0 / seen as f64,
                    d.fallbacks / 1_000_000
                );
                // F1a: 熱が閾値に達したブロック頭の数 (JITの焼き候補)
                println!("jit候補: {} ブロック頭が閾値到達", d.hot_pending());
            }
            // opstats フィーチャ付きなら、実行回数の上位を出す
            // (デコードキャッシュの対象選定は推測でなくこの実測で行う)
            let total: u64 = m.op_counts.iter().sum();
            if total > 0 {
                let mut idx: Vec<usize> = (0..512).collect();
                idx.sort_by_key(|&i| std::cmp::Reverse(m.op_counts[i]));
                println!("\n実行回数の上位 (全{}M命令):", total / 1_000_000);
                let mut cum = 0.0;
                for &i in idx.iter().take(30) {
                    let c = m.op_counts[i];
                    if c == 0 {
                        break;
                    }
                    let pct = c as f64 * 100.0 / total as f64;
                    cum += pct;
                    let name = if i < 256 {
                        format!("{:02X}", i)
                    } else {
                        format!("0F{:02X}", i - 256)
                    };
                    println!(
                        "  {name:>5}  {:>8}M  {pct:5.1}%  累積{cum:5.1}%",
                        c / 1_000_000
                    );
                }
            }
            // dcacheヒット側の動的uop分布 — JIT語彙の内外 (語彙拡大の優先度は
            // この実測で決める。カバレッジの分母はこちらが本体)
            #[cfg(feature = "opstats")]
            {
                let (inside, ref outside) = m.jit_vocab_counts;
                let out_total: u64 = outside.values().sum();
                let all = inside + out_total;
                if all > 0 {
                    println!(
                        "\nJIT語彙 (dcacheヒット側 {}M命令): 語彙内 {:.1}% / 語彙外 {:.1}%",
                        all / 1_000_000,
                        inside as f64 * 100.0 / all as f64,
                        out_total as f64 * 100.0 / all as f64
                    );
                    let mut v: Vec<(&&str, &u64)> = outside.iter().collect();
                    v.sort_by_key(|&(_, c)| std::cmp::Reverse(*c));
                    println!("語彙外の上位 (これが語彙拡大の買い物リスト):");
                    for (name, c) in v.iter().take(15) {
                        println!(
                            "  {:<28} {:>6}M  {:4.1}%",
                            name,
                            **c / 1_000_000,
                            **c as f64 * 100.0 / all as f64
                        );
                    }
                }
            }
            return;
        }
    }
    panic!("シェルに届かなかった");
}
