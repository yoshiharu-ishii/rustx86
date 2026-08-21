//! OS起動回帰 — 3つのOSをプロンプトまで起動し、画面の「スクショ」を出す。
//!
//!   cargo run --release --example regress
//!
//! CIが回すのはこれ。**「プロンプトが出た画面」こそが統合の証拠**なので、
//! 到達判定だけでなく画面そのものをレポートに残す:
//!
//!   - 16bit回帰: ELKS (login:) と FreeDOS (プロンプト) — テキストVRAMのダンプ
//!   - 32bit回帰: Linux (busybox シェル) — シリアルコンソールの末尾
//!
//! 出力は Markdown (CIのレポートにそのまま貼れる)。1つでも落ちれば exit 1。
//! イメージが無いOSは**明示的にスキップ表示** — 「動かしていないのに緑」を
//! 隠さない (ローカルの部分環境でも使えるように、スキップは失敗にしない。
//! CI側は全イメージを必ず用意するので、CIでは事実上の必須になる)。
//!
//! ## 命令数も回帰させる
//!
//! この機械は決定的なので、**同じイメージなら到達までの命令数が毎回同じ**。
//! 大きく増えたら、速度ではなく意味の後退 (スピン・二重実行・時計の狂い) を
//! 疑う印になる。上限は現測定の +30% 程度に置いてある。

use rustx86_core::{Machine, MachineProfile};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/..");

fn img(name: &str) -> String {
    format!("{ROOT}/images/{name}")
}

/// 画面 (テキストVRAM) に `needle` が出るまで回す。返り値は費やした命令数
fn run_until_screen(m: &mut Machine, needle: &str, budget: u64) -> Option<u64> {
    let start = m.cpu.tsc;
    loop {
        let spent = m.cpu.tsc.wrapping_sub(start);
        if spent >= budget {
            return None;
        }
        m.run(1_000_000);
        if m.trap.is_some() {
            return None;
        }
        if m.text_screen_string().contains(needle) {
            return Some(m.cpu.tsc.wrapping_sub(start));
        }
    }
}

/// シリアル出力に `needle` が出るまで回す
fn run_until_serial(m: &mut Machine, needle: &str, budget: u64) -> Option<u64> {
    let start = m.cpu.tsc;
    loop {
        let spent = m.cpu.tsc.wrapping_sub(start);
        if spent >= budget {
            return None;
        }
        m.run(10_000_000);
        if m.trap.is_some() {
            return None;
        }
        if String::from_utf8_lossy(&m.devices.uart.tx).contains(needle) {
            return Some(m.cpu.tsc.wrapping_sub(start));
        }
    }
}

/// 画面のスクショ (80x25のテキスト)。空行の連続は末尾だけ落とす
fn screenshot_vram(m: &Machine) -> String {
    let s = m.text_screen_string();
    s.trim_end().to_string()
}

/// シリアルの末尾 `lines` 行ぶん
fn screenshot_serial(m: &Machine, lines: usize) -> String {
    let s = String::from_utf8_lossy(&m.devices.uart.tx);
    let all: Vec<&str> = s.lines().collect();
    let from = all.len().saturating_sub(lines);
    all[from..].join("\n")
}

struct Outcome {
    name: &'static str,
    passed: Option<bool>, // None = スキップ
    detail: String,
    shot: String,
    /// 証跡としてファイルに残すログ (ファイル名, 中身)。
    /// スクショが「プロンプトの瞬間の静止画」なら、こちらは走行全体のドラレコ。
    /// ゲストの時計は決定的なので、**マージ前後のログはdiffできる回帰資料**になる
    logs: Vec<(&'static str, String)>,
}

fn report(o: &Outcome) {
    let mark = match o.passed {
        Some(true) => "✅",
        Some(false) => "❌",
        None => "⏭️ スキップ (イメージ無し)",
    };
    println!("## {} — {}\n", o.name, mark);
    if !o.detail.is_empty() {
        println!("{}\n", o.detail);
    }
    if !o.shot.is_empty() {
        println!("```text\n{}\n```\n", o.shot);
    }
}

/// 16bit回帰: ELKS がログインプロンプトまで起動する
fn elks() -> Outcome {
    let name = "16bit回帰: ELKS 0.9.1";
    let Ok(image) = std::fs::read(img("fd2880.img")) else {
        return Outcome {
            name,
            passed: None,
            detail: String::new(),
            shot: String::new(),
            logs: vec![],
        };
    };
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    let reached = run_until_screen(&mut m, "login:", 200_000_000);
    let shot = screenshot_vram(&m);
    let logs = vec![("elks-screen.txt", shot.clone())];
    match reached {
        Some(n) => Outcome {
            name,
            passed: Some(true),
            detail: format!("login: 到達 ({}M命令)", n / 1_000_000),
            shot,
            logs,
        },
        None => Outcome {
            name,
            passed: Some(false),
            detail: format!("login: に到達せず。trap={:?}", m.trap),
            shot,
            logs,
        },
    }
}

/// 16bit回帰: FreeDOS がメニューを抜けて FreeCOM のプロンプトまで起動する
fn freedos() -> Outcome {
    let name = "16bit回帰: FreeDOS 1.4";
    let Ok(image) = std::fs::read(img("fd14boot.img")) else {
        return Outcome {
            name,
            passed: None,
            detail: String::new(),
            shot: String::new(),
            logs: vec![],
        };
    };
    let mut m = Machine::new();
    m.boot_from_disk(image).expect("boot");
    let Some(n1) = run_until_screen(&mut m, "Select from Menu", 300_000_000) else {
        return Outcome {
            name,
            passed: Some(false),
            detail: format!("言語選択メニューに到達せず。trap={:?}", m.trap),
            shot: screenshot_vram(&m),
            logs: vec![("freedos-console.log", m.console_string())],
        };
    };
    // メニューをEnterで抜けると FreeCOM が立つ (BIOS経由のキー入力の検証を兼ねる)
    m.devices.keyboard.type_ascii("\n");
    let reached = run_until_screen(&mut m, "FreeCom version", 300_000_000);
    let shot = screenshot_vram(&m);
    // console は INT 10h テレタイプの全履歴 — DOSのブートログに相当する
    let logs = vec![
        ("freedos-screen.txt", shot.clone()),
        ("freedos-console.log", m.console_string()),
    ];
    match reached {
        Some(n2) => Outcome {
            name,
            passed: Some(true),
            detail: format!(
                "メニュー {}M命令 → FreeCOM {}M命令",
                n1 / 1_000_000,
                n2 / 1_000_000
            ),
            shot,
            logs,
        },
        None => Outcome {
            name,
            passed: Some(false),
            detail: format!("FreeCOMが起動せず。trap={:?}", m.trap),
            shot,
            logs,
        },
    }
}

/// 32bit回帰: Linux が busybox シェルまで起動する。
/// 決定的な命令数も見張る (大きく増えたら意味の後退を疑う)
fn linux() -> Outcome {
    let name = "32bit回帰: Linux 6.18 (Alpine)";
    // vmlinux優先 (解凍ステブ無し)。上限は現測定+30%: vmlinux 1100M → 1400M。
    // 2026-08-21 に PS/2マウスの鎖 (i2c-core/psmouse/mousedev/evdev の insmod と
    // psmouse のリセット握手) を積んで 980M → 1100M。**荷物の重さであって
    // 意味の後退ではない** (その前は TLS一式で 580M → 770M、バナー短縮で 970→980M)。
    // 2026-08-14にinitramfsへTLS一式 (ssl_client + libssl/libcrypto + CA束) を
    // 積んで 1.4→4.1MB になり、cpio展開のぶん 580M → 770M へ増えた。
    // **積んだ荷物の重さであって意味の後退ではない** — 荷物を変えたら測り直す
    let (kernel, budget) = match std::fs::read(img("vmlinux-lts")) {
        Ok(k) => (k, 1_400_000_000u64),
        Err(_) => match std::fs::read(img("vmlinuz-lts")) {
            Ok(k) => (k, 1_500_000_000u64), // bzImage 1160M → +30%
            Err(_) => {
                return Outcome {
                    name,
                    passed: None,
                    detail: String::new(),
                    shot: String::new(),
                    logs: vec![],
                }
            }
        },
    };
    let Ok(initrd) = std::fs::read(img("initramfs-mini")) else {
        return Outcome {
            name,
            passed: None,
            detail: String::new(),
            shot: String::new(),
            logs: vec![],
        };
    };
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.boot_linux_with_initrd(&kernel, "console=ttyS0", Some(&initrd))
        .expect("boot");
    let t0 = std::time::Instant::now();
    let reached = run_until_serial(&mut m, "busybox shell", budget);
    let boot_secs = t0.elapsed().as_secs_f32();
    if reached.is_some() {
        // プロンプトまでもう少し流す
        let _ = run_until_serial(&mut m, "~ #", 100_000_000);
    }
    // シリアル全文 = dmesg込みのブートログ。ゲスト時計は決定的なので
    // タイムスタンプごと diff できる回帰資料になる
    let logs = vec![(
        "linux-boot.log",
        String::from_utf8_lossy(&m.devices.uart.tx).into_owned(),
    )];
    match reached {
        Some(n) => {
            // MIPS を毎回測って記録する (perf.md の台帳と対になる時系列)。
            // 共有ランナーの壁時計は±10〜30%揺れるので、下限は「壊滅の検出」
            // だけを狙った粗い値にする — きつくするとランナーの運で赤くなり、
            // CIの信用が死ぬ。日々の推移はレポートの数字で見る
            let mips = n as f32 / 1e6 / boot_secs;
            let min_mips: f32 = std::env::var("REGRESS_MIN_MIPS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            let too_slow = min_mips > 0.0 && mips < min_mips;
            Outcome {
                name,
                passed: Some(!too_slow),
                detail: format!(
                    "シェル到達 {}M命令 / {:.1}s = **{:.1} MIPS**{} (命令数上限{}M — 決定的なので増加は意味の後退)",
                    n / 1_000_000,
                    boot_secs,
                    mips,
                    if min_mips > 0.0 {
                        if too_slow {
                            format!("。**下限 {min_mips:.0} MIPS を下回った — 壊滅的な速度後退**")
                        } else {
                            format!(" (下限 {min_mips:.0})")
                        }
                    } else {
                        String::new()
                    },
                    budget / 1_000_000
                ),
                shot: screenshot_serial(&m, 25),
                logs,
            }
        }
        None => Outcome {
            name,
            passed: Some(false),
            detail: format!(
                "シェルに到達せず (上限{}M命令)。trap={:?}",
                budget / 1_000_000,
                m.trap
            ),
            shot: screenshot_serial(&m, 25),
            logs,
        },
    }
}

/// 32bit回帰 (画面つき): リニアFBを申告して Linux を起動し、efifb が掴んで
/// fbcon が描き、ユーザー空間 (busybox fbsplash) が置いた画素がそのまま
/// LFB に現れることを見る。**申告は起動の命令数を変える**ので、素の起動
/// (上の linux()) とは別の回帰として持つ — 決定性の定規は素の方
fn linux_lfb() -> Outcome {
    let name = "32bit回帰: Linux + リニアFB (efifb)";
    let skip = || Outcome {
        name,
        passed: None,
        detail: String::new(),
        shot: String::new(),
        logs: vec![],
    };
    let Ok(kernel) = std::fs::read(img("vmlinux-lts")) else {
        return skip();
    };
    let Ok(initrd) = std::fs::read(img("initramfs-mini")) else {
        return skip();
    };
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(128));
    m.lfb_enable();
    // tty0 も console にする: 起動ログが fbcon に描かれる (実機のPCと同じ絵)。
    // 最後の console= が /dev/console なのでシェルは ttyS0 のまま
    m.boot_linux_with_initrd(&kernel, "console=tty0 console=ttyS0", Some(&initrd))
        .expect("boot");
    let budget = 1_850_000_000u64; // fbcon が描く分だけ素の起動 (1100M) より増える: 実測 1420M
    let reached = run_until_serial(&mut m, "busybox shell", budget);
    let mut checks: Vec<(&str, bool)> = vec![];
    if reached.is_some() {
        let _ = run_until_serial(&mut m, "~ #", 100_000_000);
        let serial = String::from_utf8_lossy(&m.devices.uart.tx).into_owned();
        checks.push((
            "efifb が掴んだ",
            serial.contains("efifb: framebuffer at 0x7f00000"),
        ));
        checks.push(("640x480x24", serial.contains("efifb: mode is 640x480x24")));
        checks.push((
            "fbcon が取った",
            serial.contains("Console: switching to colour frame buffer device 80x30"),
        ));
        // fbcon が起動ログを描いた = 真っ黒ではない
        let lit = m.lfb_frame().iter().filter(|&&b| b != 0).count();
        checks.push(("fbcon が文字を描いた", lit > 10_000));

        // ユーザー空間から画素を置く: 4×2 のPPMを printf で作り、busybox の
        // fbsplash で /dev/fb0 に描く。LFBにそのバイト列 (R,G,B) が現れれば、
        // /dev/fb0 → efifb → LFB の道が通っている
        let cmd = concat!(
            "printf 'P6\\n4 2\\n255\\n' > /tmp/p.ppm; ",
            "printf '\\377\\0\\0\\0\\377\\0\\0\\0\\377\\377\\377\\377",
            "\\0\\0\\0\\377\\377\\0\\377\\0\\377\\0\\377\\377' >> /tmp/p.ppm; ",
            "fbsplash -s /tmp/p.ppm; printf 'LFB%s\\n' DONE\n"
        );
        m.devices.uart.feed(cmd.as_bytes());
        let ok = run_until_serial(&mut m, "LFBDONE", 300_000_000).is_some();
        checks.push(("fbsplash が終わった", ok));
        // **busybox の fbsplash は 24bpp を B,G,R 決め打ちで書く** (var の
        // red/blue offset を見ない)。我々の申告は赤が先頭 (b8g8r8) で、fbcon と
        // efifb はそれを守るが、fbsplash だけは逆順に置く。見たいのは
        // 「ユーザー空間の書き込みが LFB に届くか」なので、fbsplash が実際に
        // 書く並び (B,G,R) で照合する (2026-08-21 に /dev/fb0 を hexdump して確認)
        let row0: [u8; 12] = [0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255];
        let row1: [u8; 12] = [0, 0, 0, 0, 255, 255, 255, 0, 255, 255, 255, 0];
        let fb = m.lfb_frame();
        let line = 640 * 3;
        let pattern_found = (0..fb.len().saturating_sub(line + 12))
            .step_by(3)
            .any(|o| fb[o..o + 12] == row0 && fb[o + line..o + line + 12] == row1);
        checks.push(("fbsplash の画素がLFBに現れた", pattern_found));

        // ユーザー空間の本物のアプリ: bounce (tools/guest/bounce-fb) が /dev/fb0 を
        // mmap して描く。stdin は /dev/null (EOFでは終わらない) にして裏で回し、
        // 1秒 (ゲスト時計) 後に止める。8色のボールが LFB に居れば、ioctl で
        // 聞いた画素形式 (赤が下位) どおりに描けている
        m.devices
            .uart
            .feed(b"bounce </dev/null & sleep 1; kill $!; printf 'BNC%s\n' DONE\n");
        let ok = run_until_serial(&mut m, "BNCDONE", 400_000_000).is_some();
        checks.push(("bounce が回って止まった", ok));
        let fb = m.lfb_frame();
        let has = |rgb: [u8; 3]| fb.chunks_exact(3).any(|p| p == rgb);
        let balls = [
            [255u8, 120, 0],
            [255, 32, 32],
            [255, 240, 32],
            [64, 240, 64],
            [64, 240, 255],
            [80, 120, 255],
            [200, 64, 255],
            [255, 160, 200],
        ];
        let n = balls.iter().filter(|c| has(**c)).count();
        checks.push(("bounce の8色がLFBに居る (R,G,Bの並びどおり)", n == 8));

        // PS/2マウス (6b): 8042 の第2ポートに psmouse が bind し、mousedev が
        // /dev/input/mice を作る。ホストから動きを1回入れ、その口から
        // PS/2形式の3バイト (ボタン/符号, dx, dy[上が正]) が出てくることを見る
        let serial = String::from_utf8_lossy(&m.devices.uart.tx).into_owned();
        checks.push(("psmouse が掴んだ", serial.contains("PS/2 Generic Mouse")));
        m.devices
            .uart
            .feed(b"head -c 3 /dev/input/mice | hexdump -C; printf 'MOU%s\n' DONE\n");
        for _ in 0..20_000_000 {
            m.step(); // head が開いて read で待つまで回す
        }
        m.devices.keyboard.mouse_motion(5, 3, 0b001); // 右5・下3・左ボタン
        let ok = run_until_serial(&mut m, "MOUDONE", 200_000_000).is_some();
        let serial = String::from_utf8_lossy(&m.devices.uart.tx).into_owned();
        // mousedev は PS/2 と同じ並びで出す: byte0 = 同期0x08 | Y負0x20 | 左0x01 = 0x29、
        // dx=05、dy=-3=fd (画面の下向き3 = PS/2 では上が正なので -3)
        checks.push((
            "/dev/input/mice に 29 05 fd",
            ok && serial.contains("29 05 fd"),
        ));
    }
    let logs = vec![(
        "linux-lfb-boot.log",
        String::from_utf8_lossy(&m.devices.uart.tx).into_owned(),
    )];
    let all_ok = reached.is_some() && checks.iter().all(|(_, ok)| *ok);
    let detail = match reached {
        Some(n) => format!(
            "シェル到達 {}M命令。{}",
            n / 1_000_000,
            checks
                .iter()
                .map(|(what, ok)| format!("{} {what}", if *ok { "✅" } else { "❌" }))
                .collect::<Vec<_>>()
                .join(" / ")
        ),
        None => format!(
            "シェルに到達せず (上限{}M命令)。trap={:?}",
            budget / 1_000_000,
            m.trap
        ),
    };
    Outcome {
        name,
        passed: Some(all_ok),
        detail,
        shot: screenshot_serial(&m, 25),
        logs,
    }
}

fn main() {
    println!("# OS起動回帰 — プロンプト到達とスクショ\n");
    let outcomes = [elks(), freedos(), linux(), linux_lfb()];
    // ブートログを証跡として regress-out/ に残す (CIがアーティファクトに上げる)
    let outdir = format!("{ROOT}/regress-out");
    let _ = std::fs::create_dir_all(&outdir);
    for o in &outcomes {
        report(o);
        for (file, content) in &o.logs {
            if let Err(e) = std::fs::write(format!("{outdir}/{file}"), content) {
                eprintln!("ログを書けない {file}: {e}");
            }
        }
    }
    let failed = outcomes.iter().filter(|o| o.passed == Some(false)).count();
    let skipped = outcomes.iter().filter(|o| o.passed.is_none()).count();
    let passed = outcomes.len() - failed - skipped;
    println!("**{passed} 合格 / {failed} 失敗 / {skipped} スキップ**");
    if failed > 0 {
        std::process::exit(1);
    }
}
