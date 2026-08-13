//! スナップショット保存/復元のIntegrationテスト (CIの「スナップショット」段)。
//!
//! 証明する不変条件は2つ:
//!
//! 1. **往復のビット不変**: `save → load → save` でバイト列が完全一致する。
//!    保存と復元のどちらかでフィールドを1つでも忘れると、ここで即座に崩れる
//! 2. **復元の透明性**: 保存点からそのまま走り続けた機械 (原本) と、
//!    別の器に復元してから走った機械 (写し) が、その後のK命令を経て
//!    **全状態ビット一致**する (`save_state` 同士の比較 = CPU・装置・
//!    メモリ・ディスクの全部)。命令数の決定性が復元を跨いで保たれる —
//!    つまり復元はゲストから観測不能である
//!
//! 対象は2機種: Linux (32bit・ページング有効の深部) と ELKS (16bit・
//! リアルモード)。実行は決定的なので「N1命令走って保存 → K命令で照合」の
//! 座標はビルドを跨いで安定する。
//!
//!   cargo run --release --example snaptest

use rustx86_core::{Machine, MachineProfile};

/// 最初の不一致点を人間可読で返す (一致ならNone)
fn diff(a: &[u8], b: &[u8]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("長さが違う: {} vs {}", a.len(), b.len()));
    }
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .map(|i| format!("+{}バイト目: {:02x} vs {:02x}", i, a[i], b[i]))
}

/// 1機種ぶんの検査。走らせ方 (boot) だけ呼び手が与える
fn check(name: &str, mut a: Machine, n1: u64, k: u64) -> bool {
    // 保存点まで走らせる (決定的なので座標は毎回同じ)
    let mut done = 0;
    while done < n1 {
        done += a.run((n1 - done).min(2_000_000));
        if let Some(t) = &a.trap {
            println!("✗ {name}: 保存点の手前でトラップ: {t:?}");
            return false;
        }
    }
    let snap1 = a.save_state();

    // 1. 往復のビット不変
    let mut b = if a.profile.ram_bytes > 1 << 20 {
        Machine::with_profile(MachineProfile::pc_32bit(a.profile.ram_bytes >> 20))
    } else {
        Machine::new()
    };
    if let Err(e) = b.load_state(&snap1) {
        println!("✗ {name}: 復元に失敗: {e}");
        return false;
    }
    let snap2 = b.save_state();
    if let Some(d) = diff(&snap1, &snap2) {
        println!("✗ {name}: save→load→save がビット一致しない ({d})");
        return false;
    }

    // 2. 復元の透明性 — 原本と写しをK命令走らせて全状態を突き合わせる
    let mut ra = 0;
    while ra < k {
        ra += a.run((k - ra).min(2_000_000));
    }
    let mut rb = 0;
    while rb < k {
        rb += b.run((k - rb).min(2_000_000));
    }
    let sa = a.save_state();
    let sb = b.save_state();
    if let Some(d) = diff(&sa, &sb) {
        println!("✗ {name}: 復元後の走行が原本と食い違う ({d}、K={k}命令)");
        return false;
    }
    println!(
        "✓ {name}: 往復ビット不変 ({} KB) + 透明性 (保存点{}M + {}M命令で全状態一致)",
        snap1.len() / 1024,
        n1 / 1_000_000,
        k / 1_000_000
    );
    true
}

fn img(name: &str) -> String {
    format!("images/{name}")
}

fn main() {
    let mut ok = true;
    let mut ran = 0;

    // Linux (32bit・ページング有効の深部で保存する — 200Mはデコンプレッサを
    // 抜けてカーネル本体、ページングもPICも動いている座標)
    if let (Ok(kernel), Ok(initrd)) = (
        std::fs::read(img("vmlinuz-lts")),
        std::fs::read(img("initramfs-mini")),
    ) {
        let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
        m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
            .expect("boot");
        ok &= check("Linux (bzImage、保存点200M)", m, 200_000_000, 20_000_000);
        ran += 1;
    } else {
        println!("- Linux: イメージが無いのでスキップ (tools/images/fetch-images.sh linux)");
    }

    // ELKS (16bit・リアルモード。50Mはログイン到達後)
    if let Ok(image) = std::fs::read(img("fd2880.img")) {
        let mut m = Machine::new();
        m.boot_from_disk(image).expect("boot");
        ok &= check("ELKS (16bit、保存点50M)", m, 50_000_000, 10_000_000);
        ran += 1;
    } else {
        println!("- ELKS: イメージが無いのでスキップ (tools/images/fetch-images.sh elks)");
    }

    if ran == 0 {
        println!("✗ 対象イメージが1つも無い — 検査になっていない");
        std::process::exit(1);
    }
    if !ok {
        std::process::exit(1);
    }
    println!("snaptest: PASS — スナップショットは往復ビット不変かつ観測不能");
}
