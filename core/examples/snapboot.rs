//! 起動済みスナップショットの検証ハーネス。
//!
//! ```bash
//! cargo run --release --example snapboot -- save   # 起動してシェルで保存
//! cargo run --release --example snapboot -- load   # 復元して ls が通るか
//! ```
//!
//! 「シンプルなカーネルの起動に1分」への即効薬は、**一度起動した機械を
//! 丸ごと控えて、次からはそこから始める**こと (Firecracker の snapshot 相当)。
//! これはその足回りが本当に一周回るかを確かめる道具。

use rustx86_core::{Machine, MachineProfile};

const SNAP: &str = "images/linux-booted.snap";

fn boot_to_shell() -> Machine {
    // vmlinux があればそちら (解凍ステブ無しで4割速い)。無ければ bzImage
    let kernel = std::fs::read("images/vmlinux-lts")
        .or_else(|_| std::fs::read("images/vmlinuz-lts"))
        .expect("images/vmlinux-lts か vmlinuz-lts");
    let initrd = std::fs::read("images/initramfs-mini").expect("images/initramfs-mini");
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
        .expect("boot");
    let t0 = std::time::Instant::now();
    // シェルのバナーが出るまで回す。番人は10G命令 — ミニinitramfsが
    // 育って (virtio/squashfs/overlayのinsmodで計8個) 3Gでは届かなくなった。
    // これは性能の門番ではなく暴走の番人なので、緩くてよい
    let mut n: u64 = 0;
    while n < 10_000_000_000 {
        let ran = m.run(50_000_000);
        n += ran;
        if m.trap.is_some() {
            panic!("trap: {:?}", m.trap);
        }
        // バナーの確認は halted の検査より**先**。シェルは出た直後にHLTで
        // 待つので、バナーを含むスライスがHLTで終わると「眠り=失敗」と
        // 誤判定してしまう (initが育ってタイミングが変わり、実際に踏んだ)
        let s = String::from_utf8_lossy(&m.devices.uart.tx);
        if s.contains("busybox shell") {
            println!(
                "シェル到達: {:.1}s ({}M命令)",
                t0.elapsed().as_secs_f32(),
                n / 1_000_000
            );
            return m;
        }
        if m.halted && ran == 0 {
            break; // 起こせない眠り (デッドハルト) — 進めなくなった
        }
    }
    panic!("シェルに届かなかった");
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "save".into());
    match mode.as_str() {
        "save" => {
            let m = boot_to_shell();
            let t0 = std::time::Instant::now();
            let snap = m.save_state();
            std::fs::write(SNAP, &snap).expect("write snap");
            println!(
                "保存: {} ({:.1}MB, {:.2}s)",
                SNAP,
                snap.len() as f64 / 1e6,
                t0.elapsed().as_secs_f32()
            );
        }
        "load" => {
            let data = std::fs::read(SNAP).expect("先に save を実行");
            let t0 = std::time::Instant::now();
            let mut m = Machine::new();
            m.load_state(&data).expect("load");
            println!("復元: {:.2}s", t0.elapsed().as_secs_f32());
            // 対話できるか: ls を打って応答を見る
            let before = m.devices.uart.tx.len();
            m.devices.uart.feed(b"ls /\n");
            let mut n: u64 = 0;
            while n < 2_000_000_000 {
                let ran = m.run(10_000_000);
                n += ran;
                if m.trap.is_some() {
                    panic!("trap: {:?}", m.trap);
                }
                // 出力の確認が halted より先 (save側と同じ理由 — 復元直後の
                // 機械はシェルのHLTで寝ていて、スライスは大抵HLTで終わる)
                let s = String::from_utf8_lossy(&m.devices.uart.tx[before..]);
                if s.contains("bin") && s.contains("dev") {
                    println!("復元後の対話OK: ls が返った ({}M命令)", n / 1_000_000);
                    println!("--- 出力 ---\n{}", s);
                    return;
                }
                if m.halted && ran == 0 {
                    break; // デッドハルト — 進めなくなった
                }
            }
            panic!("復元後に ls が返らなかった");
        }
        other => panic!("unknown mode: {other}"),
    }
}
