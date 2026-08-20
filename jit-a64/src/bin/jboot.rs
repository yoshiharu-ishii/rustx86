//! jboot — F1dの定規: JIT on/off で同じコースを走らせる。
//!
//!   cargo run --release -p rustx86-jit-a64 --bin jboot          # JIT on
//!   RUSTX86_JIT=0 cargo run --release -p rustx86-jit-a64 --bin jboot  # off
//!
//! 出力は bootphase と同じ「秒」+ 決定性の指紋 (命令数 + シリアルFNV)。
//! **JIT on/off で命令数も指紋もビット同一**が門番 (F1a以来の約束)。
//! コースは凍結された定規 (bench対、bootphaseと同じ規則)

use rustx86_core::{Machine, MachineProfile};

/// FNV-1a (シリアル出力の指紋 — jit-check.mjs と同じ趣旨)
fn fnv(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    let kernel_path = std::env::args().nth(1).unwrap_or_else(|| {
        if std::path::Path::new("images/bzImage-bench").exists() {
            "images/bzImage-bench".into()
        } else {
            "images/vmlinuz-lts".into()
        }
    });
    let initrd_path = std::env::args().nth(2).unwrap_or_else(|| {
        if std::path::Path::new("images/initramfs-bench").exists() {
            "images/initramfs-bench".into()
        } else {
            "images/initramfs-mini".into()
        }
    });
    let kernel = std::fs::read(&kernel_path).unwrap_or_else(|e| panic!("{kernel_path}: {e}"));
    let initrd = std::fs::read(&initrd_path).unwrap_or_else(|e| panic!("{initrd_path}: {e}"));
    // Machineは番地を焼き込むのでBoxで固定 (attachのSafety契約)
    let mut m = Box::new(Machine::with_profile(MachineProfile::pc_32bit(128)));
    m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
        .expect("boot");
    let jit_on = unsafe { rustx86_jit_a64::attach_if_enabled(&mut m) };
    println!(
        "コース: {kernel_path} + {initrd_path} / JIT: {}",
        if jit_on { "on" } else { "off" }
    );

    let t0 = std::time::Instant::now();
    let mut n: u64 = 0;
    // tick=256 (ADR-0025) でゲスト時間待ちの命令数が伸びる — 予算は余裕を持つ
    while n < 12_000_000_000 {
        n += m.run(10_000_000);
        if m.trap.is_some() {
            panic!("trap: {:?}", m.trap);
        }
        let s = String::from_utf8_lossy(&m.devices.uart.tx);
        if s.contains("busybox shell") {
            println!(
                "合計: {}M命令 {:.1}s",
                n / 1_000_000,
                t0.elapsed().as_secs_f32()
            );
            println!(
                "指紋: 命令数={} シリアルFNV={:016x}",
                n,
                fnv(&m.devices.uart.tx)
            );
            if jit_on {
                let (baked, rejected, installed, demoted) = rustx86_jit_a64::stats();
                println!(
                    "jit: 実行{}M命令 / 入場{}回 (平均{:.1}命令) / 焼き{} 棄却{} 据付{} 降格{}",
                    m.jit_instrs / 1_000_000,
                    m.jit_entries,
                    m.jit_instrs as f64 / m.jit_entries.max(1) as f64,
                    baked,
                    rejected,
                    installed,
                    demoted
                );
                println!("カバレッジ: {:.2}%", m.jit_instrs as f64 * 100.0 / n as f64);
            }
            return;
        }
        if m.halted {
            break;
        }
    }
    eprintln!(
        "--- シリアルの尻 (死因の現場) — n={}M halted={} ---",
        n / 1_000_000,
        m.halted
    );
    let tx = &m.devices.uart.tx;
    eprintln!(
        "{}",
        String::from_utf8_lossy(&tx[tx.len().saturating_sub(1500)..])
    );
    panic!("シェルに届かなかった");
}
