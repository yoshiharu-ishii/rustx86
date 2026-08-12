//! ブート命令の解剖 — ipを定期サンプリングして「580Mがどのカーネル関数で
//! 燃えているか」を出す計測ハーネス。
//!
//!   cargo run --release --example bootprof -- images/vmlinux-lts > /tmp/bootprof.txt
//!   uv run --with pyelftools python3 tools/bootprof-resolve.py /tmp/bootprof.txt images/vmlinux-lts
//!
//! 決定的なので同じイメージなら毎回同じヒストグラムになる。
//! サンプリングは4096命令ごと (580Mで約14万点) — E系 (実行量を減らす) の
//! 標的選定はこの実測で行う。カーネルはフラットセグメントなので ip = 線形番地。

use rustx86_core::{Machine, MachineProfile};

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "images/vmlinuz-lts".into());
    let cmdline = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "console=ttyS0".into());
    let kernel = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let initrd = std::fs::read("images/initramfs-mini").expect("images/initramfs-mini");
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_linux_with_initrd(&kernel, &cmdline, Some(&initrd))
        .expect("boot");

    const STRIDE: u64 = 4096;
    let mut samples: Vec<u32> = Vec::with_capacity(300_000);
    let mut n: u64 = 0;
    while n < 3_000_000_000 {
        n += m.run(STRIDE);
        if m.trap.is_some() {
            panic!("trap: {:?}", m.trap);
        }
        samples.push(m.cpu.ip);
        if samples.len().is_multiple_of(8192) {
            let s = String::from_utf8_lossy(&m.devices.uart.tx);
            if s.contains("busybox shell") {
                break;
            }
        }
    }
    eprintln!("合計 {}M命令 / {}サンプル", n / 1_000_000, samples.len());
    for ip in samples {
        println!("{ip:x}");
    }
}
