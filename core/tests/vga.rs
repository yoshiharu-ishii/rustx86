//! VGA mode 13h — グラフィックス画面の最初の一枚。
//!
//! 検証は3点で閉じる: (1) INT 10h でモードが入る (2) 0x3C8/0x3C9 で
//! パレットが流し込める (3) 0xA0000 に置いたバイトがそのまま画素になる。
//! FBは**ただのRAM** (書き込みフック無し) なので、(3)が通れば
//! ゲスト側の描画経路に特別なことは何も無い。

use rustx86_core::bus::{GFX_COLS, GFX_LEN};
use rustx86_core::Machine;
use rustx86_core::MachineProfile;

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

// ---------- FreeDOS を使う実物ソフトの検証 ----------

/// fd14games.img = DEBUG (lDebug) と BOUNCE.COM 入りの盤 (webのfd14boot.imgと同一物)
const FREEDOS_IMAGE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/fd14games.img");

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

/// 1文字ずつ間を空けて打つ (BIOSの待ち行列16枠を溢れさせないため)
fn type_slowly(m: &mut Machine, s: &str) {
    for ch in s.chars() {
        m.devices.keyboard.type_ascii(&ch.to_string());
        for _ in 0..1_000_000 {
            m.step();
        }
    }
}

/// F5でCONFIG/AUTOEXECを飛ばし、シェルを答えて A:\> まで
fn boot_freedos_to_prompt(image: Vec<u8>) -> Machine {
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    assert!(
        run_until(&mut m, "FreeDOS kernel", 200_000_000),
        "カーネル起動せず"
    );
    m.devices.keyboard.feed(&[0x3F, 0xBF]); // F5
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
}

/// **実物のDOSソフトが mode 13h を使う** — 1回目の起動でFreeDOSのDEBUG
/// (lDebug) が .COM をその場で組んでフロッピーに書き出し、**その盤面で
/// もう一度起動した**素のDOSがFATから読んで実行する。
/// INT 10h AH=00 → 0xA0000 へのrep stosb → INT 20h終了、の全経路が
/// 「本物のDOSが読み込んだプログラム」として通ることの証明
#[test]
fn freedos_debug_builds_and_runs_a_mode13_program() {
    let Ok(image) = std::fs::read(FREEDOS_IMAGE) else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };

    // --- 1回目の起動: DEBUGで組んでフロッピーへ書く ---
    let mut m = boot_freedos_to_prompt(image);
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
    let mut m = boot_freedos_to_prompt(written);
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

/// 色 `c` の画素の重心 (無ければ None)
fn centroid(fb: &[u8], c: u8) -> Option<(usize, usize)> {
    let (mut n, mut sx, mut sy) = (0usize, 0usize, 0usize);
    for (i, &p) in fb.iter().enumerate() {
        if p == c {
            n += 1;
            sx += i % GFX_COLS;
            sy += i / GFX_COLS;
        }
    }
    (n > 0).then(|| (sx / n, sy / n))
}

/// **BOUNCE.COM — 跳ねるボールが動き、キーでテキストへ帰る。**
/// 垂直帰線待ち (0x3DA) で進むプログラムなので、合成した帰線が
/// 「止まらず・暴走せず」一定のテンポで回ることの実地試験になっている
#[test]
fn freedos_bounce_ball_moves_and_exits_on_key() {
    let Ok(image) = std::fs::read(FREEDOS_IMAGE) else {
        eprintln!("images/fd14games.img が無いのでスキップ");
        return;
    };
    let mut m = boot_freedos_to_prompt(image);
    type_slowly(&mut m, "bounce\n");
    for _ in 0..100_000_000 {
        m.step();
        if m.video_mode == 0x13 {
            break;
        }
    }
    assert_eq!(m.video_mode, 0x13, "mode 13h に入っていない");

    // 壁 (色33) とボール (色32) が描かれるまで数フレームぶん回す
    for _ in 0..5_000_000 {
        m.step();
    }
    assert_eq!(m.framebuffer()[0], 40, "左上の壁");
    assert_eq!(
        m.devices.dac.color(32),
        [63, 30, 0],
        "ボールの橙をDACに流し込んでいる"
    );
    // 8色のボールが全部画面に居る (パレット 32..39)
    for c in 32..40u8 {
        assert!(
            centroid(&m.framebuffer(), c).is_some(),
            "色{c}のボールが居ない"
        );
    }
    let p1 = centroid(&m.framebuffer(), 32).expect("ボールが居ない");

    // 約10フレーム (1フレーム ≒ 109万命令) 進めると、ボールは別の場所に居る
    for _ in 0..11_000_000 {
        m.step();
    }
    let p2 = centroid(&m.framebuffer(), 32).expect("ボールが消えた");
    assert_ne!(p1, p2, "ボールが動いていない (帰線待ちで止まっている?)");
    let moved = p1.0.abs_diff(p2.0) + p1.1.abs_diff(p2.1);
    assert!(
        (3..=60).contains(&moved),
        "動き方がおかしい: {p1:?} → {p2:?} (帰線が速すぎ/遅すぎ)"
    );

    // キーを押すとテキストモードへ戻り、DOSのプロンプトが使える
    m.devices.keyboard.type_ascii(" ");
    for _ in 0..20_000_000 {
        m.step();
        if m.video_mode == 0x03 {
            break;
        }
    }
    assert_eq!(m.video_mode, 0x03, "キーでテキストへ戻らない");
    type_slowly(&mut m, "ver\n");
    assert!(
        run_until(&mut m, "FreeCom", 50_000_000),
        "終了後にDOSが生きていない:\n{}",
        m.text_screen_string()
    );
}

/// **DOOM が動く** — フロッピーの FreeDOS + BIOS のハードディスク (C:) に入れた
/// DOOM shareware 1.9。`DOOM` と打つと DOS/4GW が保護モードへ上がり、mode 13h に
/// タイトル画面 (パレットを積んだ絵) が出る。DOS 版 DOOM は Mode Y (チェーン4 off、
/// 4 プレーン) で描くので、合成した一枚が**横に 4 枚並んでいない**ことも見る。
/// 像は make-doom-hdd.sh の産物で、無ければ飛ばす (CI は rustx86-images から取る)。
/// DOOM_SHOT=path で画面を PPM に落とす
#[test]
fn freedos_doom_reaches_mode13_title() {
    const HDD: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../images/doom-hdd.img");
    let Ok(hdd) = std::fs::read(HDD) else {
        eprintln!("skip: {HDD} が無い");
        return;
    };
    let image = std::fs::read(FREEDOS_IMAGE).expect("fd14games.img");
    // **386 の機械で** (Machine::new() は PC/XT = 8086。DOS/16M は PUSHF の
    // bit 12-15 で世代を見るので、8086 だと「386 か 486 が要る」で止まる)
    let mut m = Machine::with_profile(MachineProfile::pc_floppy(16));
    m.boot_from_disk(image).expect("boot");
    m.hdd_attach(hdd).expect("hdd");
    assert!(run_until(&mut m, "FreeDOS kernel", 200_000_000));
    m.devices.keyboard.feed(&[0x3F, 0xBF]); // F5
    assert!(run_until(&mut m, "full shell command line", 400_000_000));
    type_slowly(&mut m, "\\FREEDOS\\BIN\\COMMAND.COM\n");
    assert!(run_until(&mut m, "A:\\>", 400_000_000));
    // C: が見える (BIOS の INT 13h ドライブ 0x80 = MBR + FAT16)
    type_slowly(&mut m, "C:\nCD \\DOOM\n");
    assert!(
        run_until(&mut m, "C:\\DOOM>", 200_000_000),
        "C: に降りられない:\n{}",
        m.text_screen_string()
    );
    type_slowly(&mut m, "DOOM\n");
    // mode 13h に入るまで回す (DOS/4GW の起動 + WAD の読み込み)
    let mut entered = false;
    for _ in 0..3_000_000_000u64 {
        m.step();
        if m.video_mode == 0x13 {
            entered = true;
            break;
        }
    }
    assert!(
        entered,
        "mode 13h に入らない。画面:\n{}\nfault={:?}",
        m.text_screen_string(),
        m.first_fault
    );
    // タイトル画面が描かれるまで進め、パレットと画素が「絵」になっていることを見る
    for _ in 0..300_000_000u64 {
        m.step();
    }
    assert!(
        m.devices.vga.planar,
        "DOOM は Mode Y (チェーン4 off) で描くはず"
    );
    let fb = m.framebuffer();
    let mut hist = [0usize; 256];
    for &p in fb.iter() {
        hist[p as usize] += 1;
    }
    let colors = hist.iter().filter(|&&n| n > 0).count();
    assert!(colors >= 32, "画素が絵になっていない (色数 {colors})");
    // 横に 4 枚並んでいない: 行 100 が周期 80 で繰り返していたらプレーンが線形に落ちている
    let row = &fb[100 * GFX_COLS..101 * GFX_COLS];
    let periodic = (0..GFX_COLS - 80)
        .filter(|&x| row[x] == row[x + 80])
        .count();
    assert!(
        periodic < (GFX_COLS - 80) / 2,
        "行 100 が周期 80 で繰り返している ({periodic}/{}): Mode Y が線形に落ちている",
        GFX_COLS - 80
    );
    // DOOM_SHOT=path で画面を PPM に落とす (ブログ・目視用)
    if let Ok(path) = std::env::var("DOOM_SHOT") {
        let pal = m.devices.dac.palette();
        let mut out = format!("P6\n{GFX_COLS} {} 255\n", GFX_LEN / GFX_COLS).into_bytes();
        for &p in fb.iter() {
            for c in 0..3 {
                out.push(pal[p as usize * 3 + c] << 2);
            }
        }
        std::fs::write(&path, out).expect("write shot");
    }
}
