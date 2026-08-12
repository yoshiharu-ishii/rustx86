//! F1c-a のハーネス: vmlinux を JIT on/off で起動して、
//! 速度 (壁時計) と決定性 (命令数 + シリアル全文ハッシュ) を出す。
//!
//!   cargo run -p rustx86-jit-native --release --bin jboot                 # JIT on
//!   RX86_JIT=0 cargo run -p rustx86-jit-native --release --bin jboot     # off
//!
//! 決定性ゲート: on/off で「命令数」と「シリアルのFNVハッシュ」が一致すること
//! (wasmの jit-check.mjs と同じ契約のネイティブ版)。
//! プレウォーミングはしない (ADR-0012 決定4) — 熱は実行の中で自然に載る

use rustx86_core::jit::JitHook;
use rustx86_core::{Machine, MachineProfile};
use rustx86_jit_native::{enter, JitRt};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "images/vmlinux-lts".into());
    let kernel = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let initrd = std::fs::read("images/initramfs-mini").expect("images/initramfs-mini");
    let jit_on = std::env::var("RX86_JIT").map_or(true, |v| v != "0");

    // Machineは動かさない (生成コードがフィールド実番地を焼き込む) — Boxで固定
    let mut m = Box::new(Machine::with_profile(MachineProfile::pc_32bit(128)));
    m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
        .expect("boot");

    let mut rt = None;
    if jit_on {
        m.jit = Some(JitHook {
            enter,
            budget_aware: true, // 生成コードがjit_budgetを毎命令照合する (F1c-c4)
        });
        rt = Some(JitRt::start());
    }

    let t0 = std::time::Instant::now();
    let mut n: u64 = 0;
    let mut serial = Vec::new();
    // ポンプ間隔はwasmと同じ2M命令 (F1b-3の実測値を引き継ぐ)
    while n < 3_000_000_000 {
        n += m.run(2_000_000);
        if let Some(t) = &m.trap {
            panic!("trap: {t:?}");
        }
        let before = serial.len();
        serial.extend_from_slice(&m.devices.uart.tx);
        m.devices.uart.tx.clear();
        if let Some(rt) = rt.as_mut() {
            rt.pump(&mut m);
        }
        // 検出は**増分だけ** (前スライス末尾12バイトを糊しろに継ぐ) — 毎スライス
        // serial全体を走査すると起動終盤で重くなる (ハーネス税の犯人だった)
        let from = before.saturating_sub(12);
        if serial[from..].windows(13).any(|w| w == b"busybox shell") {
            break;
        }
    }
    let secs = t0.elapsed().as_secs_f32();

    // FNV-1a (シリアル全文 — 決定性の指紋)
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in &serial {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }

    println!(
        "jboot: jit={} {}M命令 {:.1}s ({:.1} MIPS)",
        if jit_on { "on" } else { "off" },
        n / 1_000_000,
        secs,
        n as f64 / 1e6 / secs as f64,
    );
    println!("決定性の指紋: 命令数={n} シリアルFNV={h:016x}");
    if jit_on {
        let rt = rt.as_ref().unwrap();
        println!(
            "jit: 実行 {}M命令 / 入回数 {}M (平均ブロック長 {:.1}) 据付 {} 世代落ち {}",
            m.jit_instrs / 1_000_000,
            m.jit_entries / 1_000_000,
            m.jit_instrs as f64 / m.jit_entries.max(1) as f64,
            rt.installed,
            rt.dropped_stale,
        );
        println!("カバレッジ: {:.1}%", 100.0 * m.jit_instrs as f64 / n as f64);
        println!(
            "入場診断: 無ブロック {}M / 予算不足 {}M / IRQ保留 {}M / tick直後 {}M",
            m.jit_denied[0] / 1_000_000,
            m.jit_denied[1] / 1_000_000,
            m.jit_denied[2] / 1_000_000,
            m.jit_denied[3] / 1_000_000,
        );
    }
    // 語彙の実測 (--features opstats のときだけ)。語彙をどこへ広げるかは
    // 推測せずこの分布で決める — wasm時代 (F1b-3) と同じ流儀
    #[cfg(feature = "opstats")]
    {
        let (in_vocab, ref out) = m.jit_vocab_counts;
        let out_total: u64 = out.values().sum();
        println!(
            "uop分布: 語彙内 {:.1}% / 語彙外 {:.1}%",
            100.0 * in_vocab as f64 / (in_vocab + out_total) as f64,
            100.0 * out_total as f64 / (in_vocab + out_total) as f64
        );
        let mut v: Vec<_> = out.iter().collect();
        v.sort_by_key(|&(_, c)| std::cmp::Reverse(*c));
        for (name, c) in v.iter().take(12) {
            println!("  語彙外 {name}: {}M", **c / 1_000_000);
        }
    }
}
