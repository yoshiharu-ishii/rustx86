//! VGA mode 13h — グラフィックス画面の最初の一枚。
//!
//! 検証は3点で閉じる: (1) INT 10h でモードが入る (2) 0x3C8/0x3C9 で
//! パレットが流し込める (3) 0xA0000 に置いたバイトがそのまま画素になる。
//! FBは**ただのRAM** (書き込みフック無し) なので、(3)が通れば
//! ゲスト側の描画経路に特別なことは何も無い。

use rustx86_core::bus::{GFX_COLS, GFX_LEN};
use rustx86_core::Machine;

fn run_mode13() -> Machine {
    let path = format!("{}/../asm/mode13.bin", env!("CARGO_MANIFEST_DIR"));
    let sector = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("{path} ({e}) — nasm -f bin mode13.asm でビルド"));
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    let executed = m.run(100_000);
    assert!(m.halted, "HLT到達せず ({executed}命令)");
    m
}

/// INT 10h AX=0013h でモードが入り、BDA (0x449) にも反映される
#[test]
fn int10_enters_mode_13h() {
    let m = run_mode13();
    assert_eq!(m.video_mode, 0x13);
    assert_eq!(m.read_phys8(0x449), 0x13, "DOSソフトはBDAでモードを見る");
    assert!(m.video_modes.contains(&0x13), "要求の台帳にも残る");
}

/// 0xA0000 は装置ではなくただのRAM — 書いたバイトがそのまま画素
#[test]
fn pixels_are_plain_bytes_at_a0000() {
    let m = run_mode13();
    let fb = m.framebuffer();
    assert_eq!(fb.len(), GFX_LEN);
    // 先頭行: 0..255 のグラデーション
    for (x, &px) in fb.iter().enumerate().take(256) {
        assert_eq!(px, x as u8, "画素 ({x}, 0)");
    }
    assert_eq!(&fb[256..320], &[0u8; 64], "モード設定時にクリアされている");
    // 2行目: 色16,17,18
    assert_eq!(&fb[GFX_COLS..GFX_COLS + 3], &[16, 17, 18]);
}

/// パレットの自動歩進: 0x3C8 に一度書けば、0x3C9 への連続書きで
/// R→G→B→次の色 と勝手に進む
#[test]
fn palette_streams_with_auto_increment() {
    let m = run_mode13();
    assert_eq!(m.devices.dac.color(16), [63, 0, 0], "赤");
    assert_eq!(m.devices.dac.color(17), [0, 63, 0], "緑");
    assert_eq!(m.devices.dac.color(18), [0, 0, 63], "青");
    // 触っていない色は既定のまま (先頭16色はEGA配色)
    assert_eq!(m.devices.dac.color(1), [0, 0, 0x2A], "EGAの青");
}

/// 0x3DA の垂直帰線はtscから合成される — ポーリングすれば必ず両方の顔を見る。
/// ここが常に同じ値だと「帰線を待って描く」ゲームが永久に待つ
#[test]
fn retrace_bit_toggles_with_time() {
    let mut m = Machine::new();
    let mut seen_active = false;
    let mut seen_retrace = false;
    // 1フレーム (≒109万命令) ぶんtscを歩かせて両方の状態を踏む
    for step in 0..200u64 {
        m.cpu.tsc = step * 6000;
        let st = m.io_read8(0x3DA);
        if st & 0x08 == 0 {
            seen_active = true;
        } else {
            seen_retrace = true;
            assert_eq!(st & 0x01, 0x01, "帰線中は「表示していない」も立つ");
        }
    }
    assert!(
        seen_active && seen_retrace,
        "アクティブと帰線の両方が観測できる"
    );
}

/// mode 13h → テキストへ戻ると、テキスト画面は空白で初期化される
#[test]
fn returning_to_text_clears_the_screen() {
    let path = format!("{}/../asm/mode13.bin", env!("CARGO_MANIFEST_DIR"));
    let sector = std::fs::read(&path).unwrap();
    let mut m = Machine::new();
    m.load_boot_sector(&sector).expect("load");
    m.run(100_000);
    assert_eq!(m.video_mode, 0x13);
    // ゲストの代わりにBIOSサービスを直接叩いてテキストへ戻す
    m.cpu.regs[0] = 0x0003; // AX: AH=00 (モード設定) AL=03 (80x25テキスト)
    m.bios_interrupt(0x10);
    assert_eq!(m.video_mode, 0x03);
    let v = m.text_vram();
    assert_eq!(v[0], 0x20, "空白");
    assert_eq!(v[1], 0x07, "既定の属性");
}

/// スナップショットの往復でパレットとモードが生き残る
#[test]
fn snapshot_preserves_palette_and_mode() {
    let m = run_mode13();
    let state = m.save_state();
    let mut n = Machine::new();
    n.load_state(&state).expect("restore");
    assert_eq!(n.video_mode, 0x13);
    assert_eq!(n.devices.dac.color(17), [0, 63, 0]);
    assert_eq!(n.framebuffer()[0..4], [0, 1, 2, 3]);
}

/// **実物のDOSソフトが mode 13h を使う** — 1回目の起動でFreeDOSのDEBUG
/// (lDebug) が .COM をその場で組んでフロッピーに書き出し、**その盤面で
/// もう一度起動した**素のDOSがFATから読んで実行する。
/// INT 10h AH=00 → 0xA0000 へのrep stosb → INT 20h終了、の全経路が
/// 「本物のDOSが読み込んだプログラム」として通ることの証明
#[test]
fn freedos_debug_builds_and_runs_a_mode13_program() {
    // fd14games.img = DEBUG (lDebug) 入りの盤 (webのfd14boot.imgと同一物)
    let image_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd14games.img");
    let Ok(image) = std::fs::read(image_path) else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };

    let run_until = |m: &mut Machine, needle: &str, budget: u64| -> bool {
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
    };
    let type_slowly = |m: &mut Machine, s: &str| {
        for ch in s.chars() {
            m.devices.keyboard.type_ascii(&ch.to_string());
            for _ in 0..1_000_000 {
                m.step();
            }
        }
    };
    let boot_to_prompt = |image: Vec<u8>| -> Machine {
        let mut m = Machine::new();
        m.boot_from_disk(image).expect("boot");
        assert!(
            run_until(&mut m, "FreeDOS kernel", 200_000_000),
            "カーネル起動せず"
        );
        m.devices.keyboard.feed(&[0x3F, 0xBF]); // F5 (CONFIG/AUTOEXECを飛ばす)
        assert!(
            run_until(&mut m, "full shell command line", 400_000_000),
            "シェルの場所を聞かれない"
        );
        type_slowly(&mut m, "\\FREEDOS\\BIN\\COMMAND.COM\n");
        assert!(
            run_until(&mut m, "A:\\>", 400_000_000),
            "DOSプロンプト到達せず"
        );
        m
    };

    // --- 1回目の起動: DEBUGで組んでフロッピーへ書く ---
    let mut m = boot_to_prompt(image);
    type_slowly(&mut m, "debug\n");
    // mode 13h → 画面全部を色2で塗る → INT 20hで終了 (21バイト = 0x15)
    type_slowly(
        &mut m,
        "a\nmov ax,13\nint 10\nmov ax,a000\nmov es,ax\nxor di,di\nmov cx,fa00\nmov al,2\nrep stosb\nint 20\n\n",
    );
    type_slowly(&mut m, "n vga.com\n");
    type_slowly(&mut m, "r cx\n15\n");
    type_slowly(&mut m, "w\n");
    assert!(
        m.text_screen_string().contains("Writing"),
        "DEBUGがファイルを書けていない:\n{}",
        m.text_screen_string()
    );

    // DOSが書いた盤面ごと取り出す (VGA.COM入りのフロッピー)
    let written = m.disk.as_ref().expect("disk").data.clone();

    // --- 2回目の起動: 素のDOSがFATから読んで実行する ---
    let mut m = boot_to_prompt(written);
    type_slowly(&mut m, "vga\n");
    for _ in 0..100_000_000 {
        m.step();
        if m.video_mode == 0x13 {
            break;
        }
    }
    assert_eq!(
        m.video_mode,
        0x13,
        "mode 13h に入っていない:\n{}",
        m.text_screen_string()
    );
    // rep stosb の完走 (画面全部が色2) を待つ
    for _ in 0..20_000_000 {
        m.step();
    }
    let fb = m.framebuffer();
    assert!(
        fb.iter().all(|&b| b == 2),
        "画面が色2で塗り切れていない (先頭16画素: {:?})",
        &fb[..16]
    );
}
