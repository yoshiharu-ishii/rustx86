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
            }
            // 連結の退出理由 (opstats時のみ) — B4続編 (ページ跨ぎ連結など) の的を
            // 推測でなくこの分布で決める
            let ex = &m.dcache.chain_exits;
            let ex_total: u64 = ex.iter().sum();
            if ex_total > 0 {
                println!(
                    "連結退出: 使い切り{}M ({:.0}%) / フォールト系{}M / IRQ境界{}M / ページ跨ぎ{}M ({:.0}%)  — 平均連結長 {:.1}命令",
                    ex[0] / 1_000_000,
                    ex[0] as f64 * 100.0 / ex_total as f64,
                    ex[1] / 1_000_000,
                    ex[2] / 1_000_000,
                    ex[3] / 1_000_000,
                    ex[3] as f64 * 100.0 / ex_total as f64,
                    n as f64 / ex_total as f64
                );
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
            return;
        }
    }
    panic!("シェルに届かなかった");
}
