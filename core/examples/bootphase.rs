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
            return;
        }
    }
    panic!("シェルに届かなかった");
}
