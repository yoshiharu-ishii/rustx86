//! FreeDOS 1.4 (8086ビルド) の起動テスト。
//!
//! ディスクイメージはリポジトリに含めない (配布物なので)。
//! 置いていない環境では**黙って成功にする** — CIで落ちないようにするためだが、
//! 「動かしていないのに緑」になるので、走ったかどうかは標準エラーに出す。
//!
//! ```text
//! curl -sLO https://download.freedos.org/1.4/FD14-FloppyEdition.zip
//! unzip -j FD14-FloppyEdition.zip 144m/x86BOOT.img -d images/
//! mv images/x86BOOT.img images/fd14boot.img
//! ```
//!
//! ## ELKSと違って、こちらはBIOSを本気で使う
//!
//! ELKSは8042もVRAMも直接叩くので、BIOS層はほとんど検証されていなかった。
//! FreeDOSは `INT 10h` で描き `INT 16h` で読み `INT 13h` で読み込むので、
//! **BIOSの実装が初めて他人のコードに試される**。
//! 実際、これを動かして分かったことが2つある。
//!
//! - `INT 10h AH=0E` (テレタイプ) がテキストVRAMに書いていなかった。
//!   診断用の文字列へ積むだけだったので、BIOS越しに描くOSの画面が
//!   ブラウザに出ない状態だった
//! - `INT 10h AH=02` (カーソル移動) が空だった。DOSの画面は
//!   「カーソルを動かして書く」の組で作られるので、これが無いと何も出ない
//!
//! ## どこまで行けるか
//!
//! **BIOSではなくCPUで止まる。** 起動の途中で走るユーティリティが
//! `66 9C` (PUSHFD) から始まる386/486の判定を行うため、
//! 32bitオペランドサイズ・プレフィクス (`0x66`) が要る。これは Tier 3b の仕事なので、
//! **続きは32bit化を済ませてから**になる ([ADR-0004](../../docs/adr/0004-how-far-to-follow-the-bios.md))。

use rustx86_core::Machine;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd14boot.img");

fn run_until(m: &mut Machine, needle: &str, budget: u64) -> bool {
    for _ in 0..budget {
        if m.halted && m.pending_irq.is_none() && !m.devices.pit.counters[0].running {
            return false;
        }
        m.step();
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

/// カーネルが起動し、言語選択のメニューまで出る。
///
/// ここまでで `INT 10h` `INT 13h` `INT 16h` `INT 1Ah` が一通り通っている
#[test]
fn freedos_kernel_reaches_the_language_menu() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd14boot.img が無いのでスキップ");
        return;
    };
    assert!(
        run_until(&mut m, "Select from Menu", 200_000_000),
        "言語選択メニューに到達せず:\n{}",
        m.text_screen_string()
    );
    // カーネルの版表示はメニューが出る前に流れているので、**画面ではなく履歴**を見る。
    // `console` は INT 10h のテレタイプ出力を全部積んだもの
    let log = m.console_string();
    assert!(log.contains("FreeDOS kernel"), "カーネルの版表示が無い:\n{log}");
    assert!(log.contains("InitDisk"), "ディスクの初期化まで進んでいない:\n{log}");
}

/// メニューでEnterを打つと COMMAND.COM (FreeCOM) が起動する。
///
/// **BIOS経由のキー入力が通っている証拠**である。ELKSは8042を直接叩くので、
/// `INT 16h` がここで初めて本番になる
#[test]
fn pressing_enter_starts_freecom() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd14boot.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "Select from Menu", 200_000_000), "メニューに到達せず");
    m.devices.keyboard.type_ascii("\n");
    assert!(
        run_until(&mut m, "FreeCom version", 200_000_000),
        "FreeCOMが起動せず:\n{}",
        m.text_screen_string()
    );
}

/// ウェルカム画面のロゴが**ブロック文字で描かれている**こと。
///
/// これは表示経路の検証になっている。ロゴは `0xDB` (█) の羅列なので、
/// コードページ437の表を通していないと**画面が空に見える**。
/// 実際それで「出ているのに見えない」に一度引っかかった
#[test]
fn welcome_logo_is_drawn_with_block_characters() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd14boot.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "Select from Menu", 200_000_000), "メニューに到達せず");
    m.devices.keyboard.type_ascii("\n");
    assert!(
        run_until(&mut m, "█", 300_000_000),
        "ブロック文字のロゴが出ない:\n{}",
        m.text_screen_string()
    );
}
