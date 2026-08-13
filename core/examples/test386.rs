//! test386.asm ROM実行ハーネス — 互換ピラミッド L1 (CPU総合バイナリ)。
//!
//!   tools/images/fetch-images.sh test386   # ROMのビルド (nasm) と期待値の取得
//!   cargo run --release --example test386
//!
//! 86Box/PCem界隈の定番CPUテストROMを生で実行する。判定は2段:
//! POST 0xFF 到達 (= 検証つきテスト 0x00〜0xEE を全部通った) と、
//! EE区間のASCII出力 (シリアル) が同梱の期待値ファイルとバイト一致すること。
//! POSTの足跡が途中で止まっていたら、その番号が「落ちたテスト」を指す
//! (番号→内容の対応は test386.asm のソースと .lst を引く)

use rustx86_core::{Machine, MachineProfile};

fn main() {
    let rom_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "images/test386.bin".into());
    let ref_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "images/test386-EE-reference.txt".into());
    let rom = std::fs::read(&rom_path).unwrap_or_else(|e| panic!("{rom_path}: {e}"));
    let reference =
        std::fs::read_to_string(&ref_path).unwrap_or_else(|e| panic!("{ref_path}: {e}"));

    // 386のテストなので32bit機で走らせる (16bit機は8086を名乗る —
    // PUSHFの上位4bit等、CPUの世代がプロファイルで変わる)
    let mut m = Machine::with_profile(MachineProfile::pc_32bit(16));
    m.boot_rom(&rom).expect("boot_rom");

    // TEST386_TRACE=1 で直近命令のトレースを出す (落ちた場所を .lst と突き合わせる用)
    let tracing = std::env::var("TEST386_TRACE").is_ok();
    if tracing {
        m.dbg.on = true;
        m.dbg.trace_cap = 48;
    }

    // 完走は実測 ~数百M命令。2Gで打ち切り = エラーハンドラの無限ループ検出
    const BUDGET: u64 = 2_000_000_000;
    let mut n = 0u64;
    while n < BUDGET {
        n += m.run(1_000_000);
        if let Some(t) = &m.trap {
            println!("test386: FAIL — 未実装トラップ {t:?}");
            println!("POSTの足跡: {}", trail(&m));
            std::process::exit(1);
        }
        if m.halted {
            break;
        }
    }

    let reached_ff = m.post_trail.last() == Some(&0xFF);
    println!("test386: {}命令, POSTの足跡: {}", n, trail(&m));
    if !reached_ff {
        let last = m.post_trail.last().copied();
        println!(
            "test386: FAIL — POST 0xFF に届かず (最後のPOST: {}, halted={}, 予算切れ={})",
            last.map_or("なし".into(), |v| format!("0x{v:02X}")),
            m.halted,
            n >= BUDGET,
        );
        if tracing {
            println!("直近の実行 (古→新):");
            for s in &m.dbg.trace {
                let bytes: String = s.bytes.iter().take(8).map(|b| format!("{b:02X}")).collect();
                println!("  {:04X}:{:08X}  {}", s.cs, s.ip, bytes);
            }
        }
        std::process::exit(1);
    }

    // EE区間の照合。ROMはCRLFで出すので正規化して行単位で比べる
    let got_raw = String::from_utf8_lossy(&m.devices.uart.tx).replace("\r\n", "\n");
    let want_raw = reference.replace("\r\n", "\n");
    let got: Vec<&str> = got_raw.lines().collect();
    let want: Vec<&str> = want_raw.lines().collect();
    let mut diffs = 0;
    for i in 0..got.len().max(want.len()) {
        let g = got.get(i).copied().unwrap_or("<行なし>");
        let w = want.get(i).copied().unwrap_or("<行なし>");
        if g != w {
            if diffs < 10 {
                println!("test386: EE不一致 行{}:\n  実測: {g}\n  期待: {w}", i + 1);
            }
            diffs += 1;
        }
    }
    if diffs > 0 {
        println!(
            "test386: FAIL — EE出力 {}行中 {}行が期待値と不一致",
            got.len().max(want.len()),
            diffs
        );
        std::process::exit(1);
    }
    println!(
        "test386: PASS — 検証つきテスト全通過 (POST FF) + EE出力 {}行が期待値と一致",
        want.len()
    );
}

/// POSTの足跡を「00 01 .. EE FF」の16進列で。同じ値の連打は1つに畳む
fn trail(m: &Machine) -> String {
    let mut out = String::new();
    let mut last = None;
    for &v in &m.post_trail {
        if last == Some(v) {
            continue;
        }
        last = Some(v);
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{v:02X}"));
    }
    if out.is_empty() {
        "(なし)".into()
    } else {
        out
    }
}
