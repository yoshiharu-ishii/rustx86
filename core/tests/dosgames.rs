//! FreeDOS 上で DOS のプログラムを動かすテスト。
//!
//! **画面はテキストしか無い**ので、動くのはテキストモードのものだけである。
//! グラフィックスを要求されたら [`Machine::video_modes`] に記録が残るので、
//! 「描いていない」のか「描く先が無い」のかを区別できる。
//!
//! ## イメージの作り方
//!
//! ゲームは FreeDOS 公式のリポジトリから取る (どれもフリーソフトウェア)。
//! 起動フロッピーに置くだけで動く。
//!
//! ```text
//! # ゲームを取る
//! base=http://www.ibiblio.org/pub/micro/pc-stuff/freedos/files/repositories/1.4/games
//! for g in eliza zmiy row4; do curl -sLO "$base/$g.zip" && unzip -oq "$g.zip" -d ext; done
//!
//! # 起動フロッピーに載せる (macOS)
//! cp images/fd14boot.img images/fd14games.img
//! hdiutil attach -imagekey diskimage-class=CRawDiskImage images/fd14games.img
//! cp ext/GAMES/ELIZA/{ELIZA.EXE,RESPONSE.DAT} ext/GAMES/ZMIY/ZMIY.EXE \
//!    ext/GAMES/ROW4/ROW4T.COM /Volumes/FD14-BOOT/
//! rm -f /Volumes/FD14-BOOT/._*
//! hdiutil detach /Volumes/FD14-BOOT
//! ```

use rustx86_core::Machine;

const IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd14games.img");

fn run_until(m: &mut Machine, needle: &str, budget: u64) -> bool {
    for _ in 0..budget {
        m.step();
        if m.take_vram_dirty() && m.text_screen_string().contains(needle) {
            return true;
        }
    }
    false
}

/// 1文字ずつ間を空けて打つ。まとめると16枠のBIOS待ち行列が溢れる
fn type_slowly(m: &mut Machine, s: &str) {
    for ch in s.chars() {
        m.devices.keyboard.type_ascii(&ch.to_string());
        for _ in 0..1_000_000 {
            m.step();
        }
    }
}

/// DOSプロンプトまで進めた機械を返す。
///
/// F5 で CONFIG.SYS と AUTOEXEC.BAT を飛ばし、聞かれたシェルの場所を答える
fn dos_prompt() -> Option<Machine> {
    let image = std::fs::read(IMAGE).ok()?;
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    assert!(run_until(&mut m, "FreeDOS kernel", 200_000_000), "カーネルが起動せず");
    m.devices.keyboard.feed(&[0x3F, 0xBF]); // F5
    assert!(
        run_until(&mut m, "full shell command line", 400_000_000),
        "シェルの場所を聞かれない"
    );
    type_slowly(&mut m, "\\FREEDOS\\BIN\\COMMAND.COM\n");
    assert!(run_until(&mut m, "A:\\>", 400_000_000), "DOSプロンプトに到達せず");
    Some(m)
}

/// **ELIZA (1966) が受け答えする。**
///
/// 打った文を読み取って返してくるので、画面・キーボード・ファイル読み出しが
/// 全部通っていることの証明になる
#[test]
fn eliza_answers_back() {
    let Some(mut m) = dos_prompt() else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    type_slowly(&mut m, "eliza\n");
    assert!(run_until(&mut m, "your problem", 300_000_000), "ELIZAが起動しない");

    type_slowly(&mut m, "I am writing an emulator\n");
    assert!(
        run_until(&mut m, "you are writing an emulator", 300_000_000),
        "打った内容を読み取って返してこない:\n{}",
        m.text_screen_string()
    );
}

/// スネーク。**属性 (色) で描くゲーム**で、状態表示だけが文字で出る
#[test]
fn zmiy_draws_its_playfield() {
    let Some(mut m) = dos_prompt() else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    type_slowly(&mut m, "zmiy\n");
    assert!(
        run_until(&mut m, "SCORE:", 300_000_000),
        "状態表示が出ない:\n{}",
        m.text_screen_string()
    );
    assert!(m.text_screen_string().contains("LEVEL:"));
}

/// 四目並べ。罫線とメニューがコードページ437で描かれる
#[test]
fn row4_shows_its_menu() {
    let Some(mut m) = dos_prompt() else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    type_slowly(&mut m, "row4t\n");
    assert!(
        run_until(&mut m, "Four in a row", 300_000_000),
        "タイトルが出ない:\n{}",
        m.text_screen_string()
    );
    let screen = m.text_screen_string();
    assert!(screen.contains("1 player"), "メニューが出ていない:\n{screen}");
}

/// **グラフィックスを要求されたら記録に残る。**
///
/// HANGMAN は CGA の 320x200 (モード 0x04) を要求する。画面が白いのは
/// 「描いていない」のではなく「描く先が無い」ためで、それが分かるようにしてある。
/// 動かすには Tier 6 のフレームバッファが要る
#[test]
fn graphics_games_are_reported_not_silently_blank() {
    let Some(mut m) = dos_prompt() else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    type_slowly(&mut m, "hangman\n");
    for _ in 0..300_000_000 {
        m.step();
        if m.video_modes.contains(&0x04) {
            break;
        }
    }
    assert!(
        m.video_modes.contains(&0x04),
        "グラフィックスモードの要求が記録されていない: {:x?}",
        m.video_modes
    );
}
