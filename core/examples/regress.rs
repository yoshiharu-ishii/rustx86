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
    let Ok(image) = std::fs::read(img("fd1440.img")) else {
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
    // vmlinux優先 (解凍ステブ無し)。上限は現測定+30%: vmlinux 580M → 750M
    let (kernel, budget) = match std::fs::read(img("vmlinux-lts")) {
        Ok(k) => (k, 750_000_000u64),
        Err(_) => match std::fs::read(img("vmlinuz-lts")) {
            Ok(k) => (k, 1_300_000_000u64), // bzImage 971M → +30%
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
    let reached = run_until_serial(&mut m, "busybox shell", budget);
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
        Some(n) => Outcome {
            name,
            passed: Some(true),
            detail: format!(
                "シェル到達 ({}M命令、上限{}M — 決定的なので大きな増加は意味の後退)",
                n / 1_000_000,
                budget / 1_000_000
            ),
            shot: screenshot_serial(&m, 25),
            logs,
        },
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

fn main() {
    println!("# OS起動回帰 — プロンプト到達とスクショ\n");
    let outcomes = [elks(), freedos(), linux()];
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
