//! ELKS (16bit UNIX) の起動テスト。
//!
//! ディスクイメージはリポジトリに含めない (配布物なので) 。
//! 置いていない環境では**黙って成功にする** — CIで落ちないようにするためだが、
//! 「動かしていないのに緑」になるので、走ったかどうかは標準エラーに出す。
//!
//! ```text
//! curl -sL -o images/fd1440.img \
//!   https://github.com/ghaerr/elks/releases/download/v0.9.1/fd1440.img
//! ```
//!
//! co-simは1命令単位なので、装置の状態遷移も割り込みの受付タイミングも
//! 検証できない。**ここからは実OSを動かすこと自体がテストになる**。

use rustx86_core::Machine;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd1440.img");

/// 画面に文字列が出るまで走らせる
fn run_until(m: &mut Machine, needle: &str, budget: u64) -> bool {
    for _ in 0..budget {
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            return false;
        }
        m.step();
        // 画面を毎命令組み立ててはいけない。1命令ごとに80x25文字のStringを
        // 作ることになり、起動が数百倍遅くなる (実際にやって390秒かかった)。
        // Tier 2a で入れた dirty フラグで、**書き換わったときだけ**見る
        if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
            return true;
        }
    }
    false
}

fn boot() -> Option<Machine> {
    let image = std::fs::read(IMAGE).ok()?;
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    Some(m)
}

/// カーネルがルートをマウントし、loginプロンプトまで到達する
#[test]
fn elks_boots_to_login_prompt() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "login:", 100_000_000), "loginプロンプトに到達せず");
    let screen = m.text_screen_string();
    assert!(screen.contains("ELKS 0.9.1"), "バージョン表示が無い:\n{screen}");
    assert!(
        screen.contains("Mounted root device"),
        "ルートがマウントされていない:\n{screen}"
    );
}

/// キーボード (8042 + IRQ1) 経由でログインし、シェルが起動する
#[test]
fn elks_accepts_keyboard_login() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd1440.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "login:", 100_000_000), "loginプロンプトに到達せず");

    m.devices.keyboard.type_ascii("root\n");
    // シェルのプロンプト (#) が出るまで待つ
    let ok = run_until(&mut m, "# ", 20_000_000) || {
        let s = m.text_screen_string();
        s.lines().last().map(|l| l.trim() == "#").unwrap_or(false)
    };
    let screen = m.text_screen_string();
    assert!(screen.contains("login: root"), "入力が届いていない:\n{screen}");
    assert!(ok, "シェルのプロンプトが出ない:\n{screen}");
    assert!(m.int_counts[0x09] > 0, "キーボード割り込み (IRQ1) が起きていない");
    assert!(m.int_counts[0x80] > 0, "システムコールが呼ばれていない");
}
