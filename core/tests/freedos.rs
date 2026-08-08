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
//! Tier 3b で `0x66` を実装したことで、**インストーラの起動まで到達した**。
//! 途中で3つ踏んだ。
//!
//! - **x87 (`0xD8-0xDF`)** — 何もしないのが正しい。8087を挿していない8086では
//!   ESC命令はメモリを書き換えず、FPU判定は「書き換わらなかったこと」で不在を知る
//! - **`0x0F` 二バイト空間** — 何が来たか分かるよう、panicに2バイト目を出すようにした
//! - **`0xA9` (TEST EAX,imm32) の幅対応漏れ** — 即値を16bitで読んでIPが2バイトずれ、
//!   以後はデータを命令として食っていた。`Machine::prefixed_ops` (0x66 が付いた
//!   オペコードの記録) がこれを一発で指した

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

/// **Tier 3b の到達点**: 32bit命令を通り抜けてインストーラまで着く。
///
/// ここに来るには `0x66` プレフィクス、x87の空実装、`0xA9` の幅対応が要る。
/// どれか1つでも欠けると、IPがずれてデータを命令として食い始める
#[test]
fn reaches_the_installer_through_32bit_instructions() {
    let Some(mut m) = boot() else {
        eprintln!("images/fd14boot.img が無いのでスキップ");
        return;
    };
    assert!(run_until(&mut m, "Select from Menu", 200_000_000), "メニューに到達せず");
    m.devices.keyboard.type_ascii("\n");
    // **入力待ちのプロンプトまで**待つ。最初の行が出た時点で判定すると、
    // まだ描き終わっていないところを掴んでしまう
    assert!(
        run_until(&mut m, "[Y,N]", 600_000_000),
        "インストーラの入力待ちに到達せず:\n{}",
        m.text_screen_string()
    );
    assert!(
        m.text_screen_string().contains("installation program"),
        "インストーラの表示が無い:\n{}",
        m.text_screen_string()
    );
    // 32bit命令を実際に通っていること (通らずに着いたのなら別の道を歩いている)
    assert!(
        m.prefixed_ops.contains(&0x9C),
        "PUSHFD を通っていない。0x66 が付いたオペコード: {:x?}",
        m.prefixed_ops
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
