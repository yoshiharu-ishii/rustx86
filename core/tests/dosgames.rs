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

/// **スネーク。ハードウェアスクロールで遊ぶ。**
///
/// zmiy は 50行の盤面を常にVRAMへ描き、**見える25行の窓をCRTCの開始位置で
/// 蛇に追従させる**。ソースにそのまま書いてある:
///
/// ```c
/// if (game->screenheight < 50) {
///     int newscrolloff = game->snakeposy[0] - 12;
///     scrollscreen((newscrolloff << 6) + (newscrolloff << 4));
/// }
/// ```
///
/// これが動くには、描く側がCRTCの開始位置を見ている必要がある。
/// **見ていなかったので、画面の下が永久に出てこなかった。**
#[test]
fn zmiy_scrolls_the_window_with_the_snake() {
    let Some(mut m) = dos_prompt() else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    type_slowly(&mut m, "zmiy\n");
    assert!(run_until(&mut m, "SCORE:", 300_000_000), "起動しない");
    assert_eq!(
        m.devices.crtc.start_offset(),
        0,
        "蛇が上端付近にいる間は窓を動かさないはず"
    );

    // 下へ走らせると窓が追いかけてくる
    let mut seen = vec![];
    for _ in 0..12 {
        m.devices.keyboard.key("ArrowDown", true);
        m.devices.keyboard.key("ArrowDown", false);
        for _ in 0..40_000_000 {
            m.step();
        }
        seen.push(m.devices.crtc.start_offset());
    }
    let max = *seen.iter().max().unwrap();
    assert!(max > 0, "窓が一度も動いていない: {seen:?}");
    // 上限は 25行ぶん = 2000文字 (盤面50行 - 画面25行)。ソースの
    // `if (newscrolloff > 25) newscrolloff = 25;` と一致する
    assert_eq!(max, 2000, "窓の下限が合わない: {seen:?}");
    assert!(
        seen.windows(2).any(|w| w[1] > w[0]),
        "窓が段階的に動いていない: {seen:?}"
    );
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
